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
use cooldown_toml_util::{parse_document, replace_value, write_document};
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
/// Returns the manifest paths whose contents changed. An empty result means either no editable
/// requirement was found or every editable requirement already has the normalized target
/// constraint, so the caller should not re-lock.
///
/// # Errors
///
/// Returns a [`CoreError`] if a manifest exists but cannot be read, parsed, or written back.
#[cfg(test)]
pub fn widen_constraint(
    root: &Utf8Path,
    members: &[MemberRef],
    crate_name: &str,
    target: &str,
) -> Result<ManifestRewrite, CoreError> {
    widen_constraint_checked(root, members, crate_name, target, || Ok(()))
}

pub(crate) fn widen_constraint_checked(
    root: &Utf8Path,
    members: &[MemberRef],
    crate_name: &str,
    target: &str,
    mut validate: impl FnMut() -> Result<(), CoreError>,
) -> Result<ManifestRewrite, CoreError> {
    let mut rewrite = ManifestRewrite::default();
    let mut needs_workspace = false;
    let mut matched_member = false;
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
            validate()?;
            write_document(&abs, &doc)?;
            rewrite.modified.push(rel);
        }
        matched_member |= edits.matched;
        needs_workspace |= edits.inherited;
    }

    // An inherited entry lives in the root `[workspace.dependencies]`; the members-empty / nothing-
    // found case falls back to the root manifest (workspace table first, then its own dep sections).
    if needs_workspace || !matched_member {
        let rel = Utf8PathBuf::from("Cargo.toml");
        let abs = root.join(&rel);
        if let Some(mut doc) = parse_document(&abs)? {
            let changed = if needs_workspace {
                matches!(rewrite_workspace(&mut doc, crate_name, target), Edit::Done)
            } else {
                match rewrite_workspace(&mut doc, crate_name, target) {
                    Edit::Done => true,
                    Edit::Unchanged => false,
                    Edit::Inherited | Edit::NoVersion | Edit::NotFound => {
                        rewrite_member(&mut doc, crate_name, target).edited
                    }
                }
            };
            if changed && !rewrite.modified.iter().any(|path| path == &rel) {
                validate()?;
                write_document(&abs, &doc)?;
                rewrite.modified.push(rel);
            }
        }
    }

    Ok(rewrite)
}

/// Make the generated members' projections of `crate_name` follow a lock move from `from` to
/// `target`: in every dependency section of each manifest in `generated`, the entries that declare
/// the crate — under its own key or renamed with `package = "…"` — and whose requirement admits
/// `from` but not `target` are made to project `target` instead; everything else is left as
/// written.
///
/// A generated manifest carries one entry per compatibility line of a crate (`syn = "1"` beside
/// `syn-<hash> = { package = "syn", version = "2" }`), so the entry admitting the version that
/// moves *is* the projection of the moving node, and its siblings project nodes that stay. Nothing
/// here originates a move: the caller only follows a change some authored declaration drives, so
/// that cargo never sees the generated requirement demand a version the lock no longer carries
/// (which would resolve a second copy or fail outright).
///
/// Following is what a generator does when it recomputes the projection. When no other entry
/// projects the target line, the entry's requirement is bumped to admit `target`, with its
/// features and other fields kept verbatim — a feature the target no longer has is then cargo's
/// rejection of this change, which the caller reports like any other resolver rejection. When a
/// differently-keyed entry already projects the target line (the hash-aliased sibling for the
/// major the crate is moving *to*), a bumped entry would resolve to the very node its sibling
/// resolves to, and cargo refuses a crate that depends on one package "multiple times with
/// different names" — across every dependency section. So a sibling in the *same* section takes
/// over and the moving entry is removed, while a sibling in another section only lends its key:
/// the moving entry is renamed to it and bumped in place, keeping its own section's feature
/// contribution, since a `[dependencies]` line and a `[build-dependencies]` line are unified in
/// different contexts under resolver 2. An entry under the same key in another section is the
/// same name and simply follows too. Only crates.io entries take part: a `git`, `path`, or
/// `registry` entry of the same package name is a different source, which no crates.io move
/// touches.
///
/// Returns the manifests whose contents changed, like [`widen_constraint`].
///
/// # Errors
///
/// Returns a [`CoreError`] if a manifest exists but cannot be read, parsed, or written back.
pub fn follow_constraint(
    root: &Utf8Path,
    generated: &[MemberRef],
    crate_name: &str,
    from: &str,
    target: &str,
) -> Result<ManifestRewrite, CoreError> {
    follow_constraint_checked(root, generated, crate_name, from, target, || Ok(()))
}

pub(crate) fn follow_constraint_checked(
    root: &Utf8Path,
    generated: &[MemberRef],
    crate_name: &str,
    from: &str,
    target: &str,
    mut validate: impl FnMut() -> Result<(), CoreError>,
) -> Result<ManifestRewrite, CoreError> {
    let mut rewrite = ManifestRewrite::default();
    let mut seen: BTreeSet<Utf8PathBuf> = BTreeSet::new();
    for member in generated {
        let rel = member_manifest_rel(&member.path);
        if !seen.insert(rel.clone()) {
            continue;
        }
        let abs = root.join(&rel);
        let Some(mut doc) = parse_document(&abs)? else {
            continue;
        };
        if follow_document(&mut doc, crate_name, from, target) {
            validate()?;
            write_document(&abs, &doc)?;
            rewrite.modified.push(rel);
        }
    }
    Ok(rewrite)
}

/// One crates.io dependency entry of a manifest declaring the followed crate: where it sits and
/// what it requires.
struct ProjectedEntry {
    section: Vec<String>,
    key: String,
    requirement: String,
}

/// How one moving entry follows, decided against its siblings (see [`follow_constraint`]).
enum Follow {
    /// No other entry projects the target line: bump the requirement in place.
    Bump,
    /// A same-section sibling already projects it: that entry takes over.
    Remove,
    /// Only a sibling in another section projects it: take that sibling's key, then bump.
    RenameTo(String),
}

/// Applies [`follow_constraint`]'s edits to one manifest document; `true` when it changed.
fn follow_document(doc: &mut DocumentMut, crate_name: &str, from: &str, target: &str) -> bool {
    let entries = projected_entries(doc, crate_name);
    let admits = |entry: &ProjectedEntry, version: &str| {
        crate::version::version_in_range(&entry.requirement, version)
    };
    let mut edited = false;
    for entry in &entries {
        if !admits(entry, from) || admits(entry, target) {
            continue;
        }
        let siblings = entries
            .iter()
            .filter(|other| other.key != entry.key && admits(other, target));
        let follow = match siblings.min_by_key(|other| other.section != entry.section) {
            None => Follow::Bump,
            Some(sibling) if sibling.section == entry.section => Follow::Remove,
            Some(sibling) => Follow::RenameTo(sibling.key.clone()),
        };
        let keys: Vec<&str> = entry.section.iter().map(String::as_str).collect();
        let Some(table) = navigate_mut(doc, &keys) else {
            continue;
        };
        match follow {
            Follow::Bump => {
                if let Some(item) = table.get_mut(&entry.key) {
                    edited |= matches!(rewrite_dep_item(item, target), Edit::Done);
                }
            }
            Follow::Remove => edited |= table.remove(&entry.key).is_some(),
            Follow::RenameTo(key) => {
                if let Some(mut item) = table.remove(&entry.key) {
                    // Under a key that is not the crate's name, cargo requests the package the
                    // key spells unless `package` says otherwise: a plain `syn = "1"` renamed
                    // must become `syn-new = { package = "syn", … }`, never a request for a
                    // registry crate called `syn-new`.
                    name_package(&mut item, crate_name);
                    rewrite_dep_item(&mut item, target);
                    table.insert(&key, item);
                    edited = true;
                }
            }
        }
    }
    edited
}

/// Makes a dependency entry name its package explicitly (`package = "<crate_name>"`), turning a
/// bare requirement into an inline table first, so the entry keeps requesting the same crate
/// whatever key it sits under.
fn name_package(item: &mut Item, crate_name: &str) {
    if let Some(requirement) = item.as_str().map(str::to_owned) {
        let mut table = toml_edit::InlineTable::new();
        table.insert("version", toml_edit::Value::from(requirement));
        replace_value(item, table);
    }
    if let Some(table) = item.as_table_like_mut()
        && table.get("package").is_none()
    {
        table.insert("package", toml_edit::value(crate_name));
    }
    // An insertion into an inline table inherits the previous value's trailing decor; re-space
    // the table so the entry reads as a generator would write it.
    if let Some(inline) = item.as_inline_table_mut() {
        inline.fmt();
    }
}

/// Every crates.io entry in every dependency section of `doc` that declares `crate_name` with a
/// version requirement of its own: an inherited entry's requirement is the workspace's, and a
/// `git`, `path`, or `registry` entry is another source, whose `version` (cargo checks it against
/// the checkout) no crates.io move may touch.
fn projected_entries(doc: &DocumentMut, crate_name: &str) -> Vec<ProjectedEntry> {
    let mut entries = Vec::new();
    for section in dependency_section_paths(doc) {
        let Some(table) = navigate(doc, &section) else {
            continue;
        };
        for (key, item) in table.iter() {
            if declared_package(key, item) != crate_name || !is_crates_io_entry(item) {
                continue;
            }
            if let Some(requirement) = declared_requirement(item) {
                entries.push(ProjectedEntry {
                    section: section.clone(),
                    key: key.to_string(),
                    requirement,
                });
            }
        }
    }
    entries
}

/// Whether a dependency entry resolves from crates.io: a bare requirement, or a table naming no
/// other source.
fn is_crates_io_entry(item: &Item) -> bool {
    item.as_table_like().is_none_or(|table| {
        ["git", "path", "registry", "registry-index"]
            .iter()
            .all(|source| table.get(source).is_none())
    })
}

/// The package a dependency entry declares: its `package = "…"` rename target, else its key.
fn declared_package<'entry>(key: &'entry str, item: &'entry Item) -> &'entry str {
    item.as_table_like()
        .and_then(|table| table.get("package"))
        .and_then(Item::as_str)
        .unwrap_or(key)
}

/// A dependency entry's own version requirement: the bare string, or the table's `version`
/// field. `None` for an inherited (`workspace = true`), path, or git entry, which the generated
/// manifest does not own the requirement of.
fn declared_requirement(item: &Item) -> Option<String> {
    if let Some(requirement) = item.as_str() {
        return Some(requirement.to_string());
    }
    let table = item.as_table_like()?;
    if table.get("workspace").and_then(Item::as_bool) == Some(true) {
        return None;
    }
    table
        .get("version")
        .and_then(Item::as_str)
        .map(str::to_string)
}

/// The project-root-relative path of a member's `Cargo.toml` (`.` is the root crate).
pub(crate) fn member_manifest_rel(member_path: &str) -> Utf8PathBuf {
    if member_path.is_empty() || member_path == "." {
        Utf8PathBuf::from("Cargo.toml")
    } else {
        // Not `Utf8Path::join`, which spells the native separator: this path is reported (a
        // diagnostic's `path`, the regeneration note), and `/` separates on Windows as well.
        Utf8PathBuf::from(format!("{member_path}/Cargo.toml"))
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
    /// The requirement already has the normalized target constraint.
    Unchanged,
    /// The requirement was rewritten in place.
    Done,
}

/// What rewriting one member manifest changed, aggregated across all of its dependency sections.
#[derive(Default)]
struct MemberEdits {
    /// At least one editable requirement was found.
    matched: bool,
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
            Edit::Done => {
                edits.matched = true;
                edits.edited = true;
            }
            Edit::Unchanged => edits.matched = true,
            Edit::Inherited => edits.inherited = true,
            Edit::NoVersion | Edit::NotFound => {}
        }
    }
    edits
}

/// Rewrite a crate's requirement in the root `[workspace.dependencies]` table, if present.
fn rewrite_workspace(doc: &mut DocumentMut, crate_name: &str, target: &str) -> Edit {
    rewrite_entry(doc, &["workspace", "dependencies"], crate_name, target)
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
        let bumped = bump_req(&req, target);
        if bumped == req {
            return Edit::Unchanged;
        }
        replace_value(item, bumped);
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
        let bumped = bump_req(&req, target);
        if bumped == req {
            return Edit::Unchanged;
        }
        replace_value(version, bumped);
        return Edit::Done;
    }
    Edit::NoVersion
}

/// Descend `path` into a table-like node, or `None` if any segment is missing or not a table.
fn navigate<'doc>(doc: &'doc DocumentMut, path: &[String]) -> Option<&'doc dyn TableLike> {
    let mut table: &dyn TableLike = doc.as_table();
    for key in path {
        table = table.get(key)?.as_table_like()?;
    }
    Some(table)
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
    fn already_widened_member_is_unchanged_without_root_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("crates/app")).expect("mkdir");
        let root_manifest = indoc::indoc! {r#"
            [workspace]
            members = ["crates/app"]

            [workspace.dependencies]
            serde = "1"
        "#};
        std::fs::write(root.join("Cargo.toml"), root_manifest).expect("write root");
        let member_manifest = indoc::indoc! {r#"
            [package]
            name = "app"

            [dependencies]
            serde = "2.3.0"
        "#};
        std::fs::write(root.join("crates/app/Cargo.toml"), member_manifest).expect("write member");

        let rewrite = widen_constraint(
            root,
            &[member("app", "crates/app")],
            "serde",
            "2.3.0+build.1",
        )
        .expect("widen");

        assert!(rewrite.modified.is_empty());
        let root_after = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root");
        assert_eq!(root_after, root_manifest);
        let member_after =
            std::fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read member");
        assert_eq!(member_after, member_manifest);
    }

    #[test]
    fn rewrite_keeps_trailing_comments() {
        // A trailing comment is often the only record of why a requirement is what it is
        // (`html5ever = "0.39"  # must match markup5ever_rcdom`), so a bump must carry it along.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("crates/app")).expect("mkdir");
        std::fs::write(
            root.join("Cargo.toml"),
            indoc::indoc! {r#"
                [workspace]
                members = ["crates/app"]

                [workspace.dependencies]
                serde = "1"         # must match serde_derive
                toml = { version = "0.5", features = ["preserve_order"] } # pinned by the parser
            "#},
        )
        .expect("write root");
        std::fs::write(
            root.join("crates/app/Cargo.toml"),
            indoc::indoc! {r#"
                [package]
                name = "app"

                [dependencies]
                serde.workspace = true
                toml.workspace = true
            "#},
        )
        .expect("write member");

        widen_constraint(root, &[member("app", "crates/app")], "serde", "2.3.0").expect("widen");
        widen_constraint(root, &[member("app", "crates/app")], "toml", "1.1.2").expect("widen");

        let after = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root");
        assert!(
            after.contains("serde = \"2.3.0\"         # must match serde_derive\n"),
            "{after}"
        );
        assert!(
            after.contains(
                "toml = { version = \"1.1.2\", features = [\"preserve_order\"] } # pinned by the parser\n"
            ),
            "{after}"
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
        let repeated =
            widen_constraint(root, &[member("root", ".")], "serde", "2.3.0").expect("widen again");
        assert!(repeated.modified.is_empty());
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

    /// A hakari-shaped generated manifest: two hash-aliased lines of one crate, a plain-key line,
    /// and an inherited line.
    const HACK_MANIFEST: &str = indoc::indoc! {r#"
        [package]
        name = "workspace-hack"

        ### BEGIN HAKARI SECTION
        [dependencies]
        hashbrown-3575ec1268b04181 = { package = "hashbrown", version = "0.15", features = ["serde"] }
        hashbrown-582f2526e08bb6a0 = { package = "hashbrown", version = "0.14", default-features = false, features = ["raw"] }
        itertools = { version = "0.13", features = ["use_std"] }
        serde = { workspace = true }

        [build-dependencies]
        itertools = { version = "0.13" }
        ### END HAKARI SECTION
    "#};

    fn hack_fixture() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8").to_owned();
        std::fs::create_dir_all(root.join("crates/workspace-hack")).expect("mkdir");
        std::fs::write(root.join("crates/workspace-hack/Cargo.toml"), HACK_MANIFEST)
            .expect("write");
        (dir, root)
    }

    #[test]
    fn follow_rewrites_only_the_aliased_line_that_projects_the_moving_version() {
        let (_dir, root) = hack_fixture();
        let hack = member("workspace-hack", "crates/workspace-hack");

        let rewrite =
            follow_constraint(&root, &[hack], "hashbrown", "0.15.5", "0.17.1").expect("follow");

        assert_eq!(
            rewrite.modified,
            vec![Utf8PathBuf::from("crates/workspace-hack/Cargo.toml")]
        );
        let after =
            std::fs::read_to_string(root.join("crates/workspace-hack/Cargo.toml")).expect("read");
        assert!(
            after.contains(
                r#"hashbrown-3575ec1268b04181 = { package = "hashbrown", version = "0.17.1", features = ["serde"] }"#
            ),
            "the 0.15 projection follows, features kept: {after}"
        );
        assert!(
            after.contains(r#"version = "0.14", default-features = false, features = ["raw"]"#),
            "the 0.14 line projects a node that did not move: {after}"
        );
        assert!(
            after.contains("### BEGIN HAKARI SECTION"),
            "the generator's markers survive: {after}"
        );
    }

    /// The moving line's target is one a sibling entry already projects: bumped, both entries
    /// would resolve to the same node under different names, which cargo refuses, so the
    /// moving entry is dropped and the sibling carries the line — as a regeneration would.
    #[test]
    fn follow_removes_the_moving_entry_when_a_sibling_projects_the_target() {
        let (_dir, root) = hack_fixture();
        let hack = member("workspace-hack", "crates/workspace-hack");

        let rewrite =
            follow_constraint(&root, &[hack], "hashbrown", "0.14.5", "0.15.5").expect("follow");

        assert_eq!(
            rewrite.modified,
            vec![Utf8PathBuf::from("crates/workspace-hack/Cargo.toml")]
        );
        let after =
            std::fs::read_to_string(root.join("crates/workspace-hack/Cargo.toml")).expect("read");
        assert!(
            !after.contains("hashbrown-582f2526e08bb6a0"),
            "the 0.14 projection is gone: {after}"
        );
        assert!(
            after.contains(
                r#"hashbrown-3575ec1268b04181 = { package = "hashbrown", version = "0.15", features = ["serde"] }"#
            ),
            "the sibling projecting the target line is untouched: {after}"
        );
        assert!(after.contains("itertools = { version = \"0.13\", features = [\"use_std\"] }"));
    }

    /// A sibling that projects the target line in *another* section does not take over that
    /// section's contribution: the moving entry keeps its section and features, borrowing only
    /// the sibling's key so the two names cannot collide on one node.
    #[test]
    fn follow_renames_to_a_sibling_in_another_section_and_bumps_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("hack")).expect("mkdir");
        std::fs::write(
            root.join("hack/Cargo.toml"),
            indoc::indoc! {r#"
                [dependencies]
                syn-old = { package = "syn", version = "1", features = ["full"] }

                [build-dependencies]
                syn-new = { package = "syn", version = "2", features = ["derive"] }
            "#},
        )
        .expect("write");

        follow_constraint(root, &[member("hack", "hack")], "syn", "1.0.109", "2.0.50")
            .expect("follow");

        let after = std::fs::read_to_string(root.join("hack/Cargo.toml")).expect("read");
        assert!(
            after.contains(
                r#"syn-new = { package = "syn", version = "2.0.50", features = ["full"] }"#
            ),
            "the normal-dependency line keeps its features under the shared name: {after}"
        );
        assert!(!after.contains("syn-old"), "{after}");
        assert!(
            after
                .contains(r#"syn-new = { package = "syn", version = "2", features = ["derive"] }"#),
            "the build-dependency line is untouched: {after}"
        );
    }

    /// A plain entry renamed to a sibling's key must still request its own crate: the rename
    /// adds `package = "<crate>"` (turning a bare requirement into a table), or cargo would
    /// request a registry crate spelled like the alias.
    #[test]
    fn follow_rename_keeps_the_package_identity_of_a_plain_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("hack")).expect("mkdir");
        std::fs::write(
            root.join("hack/Cargo.toml"),
            indoc::indoc! {r#"
                [dependencies]
                syn = "1"
                serde = { version = "1", features = ["derive"] }

                [build-dependencies]
                syn-new = { package = "syn", version = "2" }
                serde-new = { package = "serde", version = "2" }
            "#},
        )
        .expect("write");
        let hack = member("hack", "hack");

        follow_constraint(
            root,
            std::slice::from_ref(&hack),
            "syn",
            "1.0.109",
            "2.0.50",
        )
        .expect("follow syn");
        follow_constraint(
            root,
            std::slice::from_ref(&hack),
            "serde",
            "1.0.200",
            "2.0.0",
        )
        .expect("follow serde");

        let after = std::fs::read_to_string(root.join("hack/Cargo.toml")).expect("read");
        assert!(
            after.contains(r#"syn-new = { version = "2.0.50", package = "syn" }"#),
            "a bare requirement becomes a table naming its crate: {after}"
        );
        assert!(
            after.contains(
                r#"serde-new = { version = "2.0.0", features = ["derive"], package = "serde" }"#
            ),
            "a table without `package` gains it: {after}"
        );
        assert!(
            !after.contains("\nsyn = ") && !after.contains("\nserde = "),
            "the plain keys are gone: {after}"
        );
    }

    /// A `git`, `path`, or `registry` entry of the same package name is another source: a
    /// crates.io move never touches it, and it never counts as a sibling.
    #[test]
    fn follow_ignores_entries_from_other_sources() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("hack")).expect("mkdir");
        let manifest = indoc::indoc! {r#"
            [dependencies]
            foo-git = { package = "foo", git = "https://example.com/foo", rev = "abc", version = "1" }
            foo-private = { package = "foo", registry = "internal", version = "2" }
            foo = "1"
        "#};
        std::fs::write(root.join("hack/Cargo.toml"), manifest).expect("write");

        follow_constraint(root, &[member("hack", "hack")], "foo", "1.0.0", "2.0.0")
            .expect("follow");

        let after = std::fs::read_to_string(root.join("hack/Cargo.toml")).expect("read");
        assert!(
            after.contains(r#"foo = "2.0.0""#),
            "the crates.io line follows: {after}"
        );
        assert!(
            after.contains(r#"rev = "abc", version = "1""#)
                && after.contains(r#"registry = "internal", version = "2""#),
            "other sources are as written: {after}"
        );
    }

    #[test]
    fn follow_rewrites_every_section_and_keeps_inherited_entries() {
        let (_dir, root) = hack_fixture();
        let hack = member("workspace-hack", "crates/workspace-hack");

        follow_constraint(
            &root,
            std::slice::from_ref(&hack),
            "itertools",
            "0.13.0",
            "0.14.0",
        )
        .expect("follow");
        let after =
            std::fs::read_to_string(root.join("crates/workspace-hack/Cargo.toml")).expect("read");
        assert_eq!(
            after.matches(r#"version = "0.14.0""#).count(),
            2,
            "both the normal and the build-dependency line follow: {after}"
        );
        assert!(
            after.contains(r#"features = ["use_std"]"#),
            "features kept: {after}"
        );

        // An inherited entry is the workspace's requirement, not the generated manifest's.
        let rewrite =
            follow_constraint(&root, &[hack], "serde", "1.0.0", "2.0.0").expect("follow serde");
        assert!(rewrite.modified.is_empty());
        let unchanged =
            std::fs::read_to_string(root.join("crates/workspace-hack/Cargo.toml")).expect("read");
        assert!(
            unchanged.contains("serde = { workspace = true }"),
            "{unchanged}"
        );
    }

    #[test]
    fn follow_is_a_no_op_when_nothing_projects_the_moving_version() {
        let (_dir, root) = hack_fixture();
        let hack = member("workspace-hack", "crates/workspace-hack");

        // A move within a line the projection already admits changes nothing …
        let within = follow_constraint(
            &root,
            std::slice::from_ref(&hack),
            "hashbrown",
            "0.15.2",
            "0.15.5",
        )
        .expect("follow");
        assert!(within.modified.is_empty());
        // … nor does a crate the manifest never mentions, or a version no line admits.
        let absent = follow_constraint(
            &root,
            std::slice::from_ref(&hack),
            "tokio",
            "1.0.0",
            "2.0.0",
        )
        .expect("follow");
        assert!(absent.modified.is_empty());
        let other_line =
            follow_constraint(&root, &[hack], "hashbrown", "0.12.0", "0.17.1").expect("follow");
        assert!(other_line.modified.is_empty());
        let unchanged =
            std::fs::read_to_string(root.join("crates/workspace-hack/Cargo.toml")).expect("read");
        assert_eq!(unchanged, HACK_MANIFEST);
    }

    /// The authored widen matches a manifest key, so the hash-aliased line is invisible to it —
    /// the gap the follower closes for generated members.
    #[test]
    fn widen_does_not_see_a_renamed_entry() {
        let (_dir, root) = hack_fixture();
        let hack = member("workspace-hack", "crates/workspace-hack");
        let rewrite = widen_constraint(&root, &[hack], "hashbrown", "0.17.1").expect("widen");
        assert!(rewrite.modified.is_empty());
    }
}
