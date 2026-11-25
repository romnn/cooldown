//! The workspace members and declarations `bun.lock` records, compared with the live manifests.
//!
//! Bun's frozen check passes a lock whose members or member versions no longer match, so its
//! exit 0 proves the lock current only when the members the root `workspaces` patterns match are
//! exactly the ones the lock records, and every recorded row still carries the live package's
//! name, a member's version, and its declarations in Bun's representation.

use super::LOCKFILE;
use crate::lock::is_workspace_relative;
use camino::Utf8Path;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Why Bun's exit 0 would not prove `lock` current for the project at the canonical `root`, whose
/// root `manifest` is given, if it would not.
pub(super) fn unproven_by_bun(root: &Utf8Path, manifest: &Value, lock: &Value) -> Option<String> {
    let relock = |subject: &str| {
        format!("{subject} differs from what {LOCKFILE} recorded; run `bun install`")
    };
    let rows = lock.get("workspaces").and_then(Value::as_object);
    let Some((root_row, rows)) = rows.and_then(|rows| Some((rows.get("")?, rows))) else {
        return Some(format!(
            "{LOCKFILE} records no root workspace; run `bun install`"
        ));
    };
    let live = match member_paths(root, manifest) {
        Ok(live) => live,
        Err(reason) => return Some(reason),
    };
    let mut recorded = BTreeSet::new();
    for key in rows.keys().filter(|key| !key.is_empty()) {
        // A recorded member is read from disk, so a key that could address a manifest outside
        // the project is refused.
        if !is_workspace_relative(key) {
            return Some(format!(
                "{LOCKFILE} records the workspace member `{key}` outside the project"
            ));
        }
        recorded.insert(key.clone());
    }
    if live != recorded {
        return Some(relock("the set of workspace members"));
    }
    if !same_package(manifest, root_row, false) {
        return Some(relock("package.json"));
    }
    for (key, row) in rows.iter().filter(|(key, _)| !key.is_empty()) {
        let manifest = format!("{key}/package.json");
        let Some(declared) = super::read_manifest(&root.join(&manifest)) else {
            return Some(format!("{manifest} cannot be read as JSON"));
        };
        if !same_package(&declared, row, true) {
            return Some(relock(&manifest));
        }
    }
    None
}

/// The `/`-separated member directories the root `manifest`'s `workspaces` patterns match, as
/// Bun 1.4.5 matches them, or why they cannot be established.
///
/// Two pattern forms are supported, each with an optional leading `./` and trailing `/`:
///
/// - a literal path, a member when it holds a `package.json`;
/// - a literal path followed by a final `*`, matching each child directory, through symbolic
///   links, that holds a `package.json`, except `node_modules` and names starting with `.`.
///
/// Every other form, such as `**`, a wildcard inside a name, braces, or a negation, is refused,
/// since Bun's matching of them was not established.
fn member_paths(root: &Utf8Path, manifest: &Value) -> Result<BTreeSet<String>, String> {
    let patterns = match manifest.get("workspaces") {
        None | Some(Value::Null) => return Ok(BTreeSet::new()),
        Some(Value::Object(object)) => object.get("packages").unwrap_or(&Value::Null),
        Some(patterns) => patterns,
    };
    let patterns = match patterns {
        Value::Null => return Ok(BTreeSet::new()),
        Value::Array(patterns) => patterns,
        _ => return Err("package.json's `workspaces` is no list of patterns".to_string()),
    };
    let mut members = BTreeSet::new();
    for pattern in patterns {
        let unsupported =
            || format!("package.json has the unsupported workspace pattern {pattern}");
        let mut rest = pattern.as_str().ok_or_else(unsupported)?;
        while let Some(stripped) = rest.strip_prefix("./") {
            rest = stripped;
        }
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let (directory, wildcard) = match rest.strip_suffix("/*") {
            Some(directory) => (directory, true),
            None if rest == "*" => ("", true),
            None => (rest, false),
        };
        // Only a final `*` may follow the root itself; a literal path must name a directory below.
        let literal = if directory.is_empty() {
            wildcard
        } else {
            directory != "." && is_workspace_relative(directory)
        };
        if !literal || directory.contains(['*', '?', '[', ']', '{', '}', '(', ')', '!', '\\']) {
            return Err(unsupported());
        }
        if !wildcard {
            if root.join(directory).join("package.json").is_file() {
                members.insert(directory.to_string());
            }
            continue;
        }
        let entries = match std::fs::read_dir(root.join(directory)) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(format!("{directory} cannot be listed: {err}")),
        };
        for entry in entries {
            let entry = entry.map_err(|err| format!("{directory} cannot be listed: {err}"))?;
            let Ok(name) = entry.file_name().into_string() else {
                return Err(format!(
                    "{directory} holds an entry whose name is not UTF-8"
                ));
            };
            let child = root.join(directory).join(&name);
            if name.starts_with('.')
                || name == "node_modules"
                || !child.is_dir()
                || !child.join("package.json").is_file()
            {
                continue;
            }
            members.insert(if directory.is_empty() {
                name
            } else {
                format!("{directory}/{name}")
            });
        }
    }
    Ok(members)
}

/// Whether the `manifest` of a package declares what its `bun.lock` workspace `row` recorded: its
/// name, a `member`'s version, and its declarations.
///
/// Bun records no version for the root.
fn same_package(manifest: &Value, row: &Value, member: bool) -> bool {
    let text = |document: &Value, field: &str| {
        document
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let optional_peers = manifest
        .get("peerDependenciesMeta")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, meta)| meta.get("optional") == Some(&Value::Bool(true)))
        .map(|(name, _)| name.clone())
        .collect();
    let recorded_optional_peers = row
        .get("optionalPeers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_string))
        .collect();
    text(manifest, "name") == text(row, "name")
        && (!member || text(manifest, "version") == text(row, "version"))
        && declarations(manifest, optional_peers) == declarations(row, recorded_optional_peers)
}

/// A package's install declarations as `bun.lock` records them.
#[derive(Debug, PartialEq, Eq)]
struct Declarations {
    dependencies: Map<String, Value>,
    dev_dependencies: Map<String, Value>,
    optional_dependencies: Map<String, Value>,
    peer_dependencies: Map<String, Value>,
    optional_peers: BTreeSet<String>,
}

/// The `package`'s declarations with its `optional_peers`, normalized the way Bun records them.
///
/// A name `optionalDependencies` declares is left out of `dependencies`, and an optional peer
/// that `peerDependencies` does not declare is recorded as the peer `*`.
/// Both normalizations leave a lock row as it is, so a manifest and its row compare equal.
fn declarations(package: &Value, optional_peers: BTreeSet<String>) -> Declarations {
    let field = |name: &str| {
        package
            .get(name)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let optional_dependencies = field("optionalDependencies");
    let mut dependencies = field("dependencies");
    dependencies.retain(|name, _| !optional_dependencies.contains_key(name));
    let mut peer_dependencies = field("peerDependencies");
    for name in &optional_peers {
        peer_dependencies
            .entry(name.clone())
            .or_insert_with(|| Value::String("*".to_string()));
    }
    Declarations {
        dependencies,
        dev_dependencies: field("devDependencies"),
        optional_dependencies,
        peer_dependencies,
        optional_peers,
    }
}
