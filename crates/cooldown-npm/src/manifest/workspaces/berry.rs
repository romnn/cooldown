//! The members of a Yarn Berry workspace and the packages its root `resolutions` can rewrite.
//!
//! A Berry lock records an entry for every workspace Yarn installed, so the members are the
//! workspaces the lock lists.
//! Discovering them from the manifests instead would mean porting Berry's own discovery: each
//! workspace's `workspaces` field globbed recursively with fast-glob's semantics, any directory
//! holding a `package.json` counted, named or not ([Workspace.ts][workspace]).
//! A workspace on disk that the lock does not list yet has no binding in it either, and only
//! verifying the lock's currency tells that the lock is stale.
//!
//! [workspace]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/Workspace.ts

use super::{Resolutions, read_text};
use crate::lock::{NodeLock, YarnBerry};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::Value;
use std::collections::BTreeSet;

/// The project-relative directory of every workspace the `yarn.lock` under `root` records, the
/// root's excluded, or why the lock records none Yarn would read.
pub(super) fn locked_directories(root: &Utf8Path) -> Result<Vec<Utf8PathBuf>, String> {
    let content = read_text(root, Utf8Path::new(YarnBerry::LOCKFILE))?
        .ok_or_else(|| "yarn.lock does not exist".to_string())?;
    let paths = crate::lock::berry_workspace_paths(&content).map_err(|err| err.to_string())?;
    if !paths.iter().any(|path| path == ".") {
        return Err("yarn.lock records no root workspace".to_string());
    }
    Ok(paths
        .iter()
        .filter(|path| *path != ".")
        .map(|path| path.split('/').collect())
        .collect())
}

/// The packages the root manifest's `resolutions` can rewrite in a Berry workspace.
///
/// Yarn rewrites a dependency through a resolution only when the dependency's name is the one the
/// resolution key targets; a parent qualifier or a range only narrows which of its descriptors are
/// rewritten ([CorePlugin.ts][core]).
/// A key cooldown cannot read, and a field that is no object, could target any package.
///
/// [core]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/CorePlugin.ts
pub(super) fn resolutions(root_document: &Value) -> Resolutions {
    let entries = match root_document.get("resolutions") {
        None | Some(Value::Null) => return Resolutions::Names(BTreeSet::new()),
        Some(Value::Object(entries)) => entries,
        Some(_) => return Resolutions::Opaque,
    };
    let mut names = BTreeSet::new();
    for key in entries.keys() {
        let Some(name) = resolution_target(key) else {
            return Resolutions::Opaque;
        };
        names.insert(name.to_string());
    }
    Resolutions::Names(names)
}

/// The package a Berry `resolutions` key targets, or `None` for a key Yarn refuses.
///
/// The key is read by a port of Yarn's resolution grammar ([resolution.pegjs][grammar]): an
/// optional parent specifier and `/`, then the target specifier, each a package name optionally
/// followed by `@` and a description.
/// A name is `@scope/name` or a run without `/` or `@`, and a description is a run without `/`.
/// A key opening with a `*` or `**` glob segment is refused, as Yarn refuses it
/// ([resolution.ts][legacy]).
///
/// [grammar]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-parsers/sources/grammars/resolution.pegjs
/// [legacy]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-parsers/sources/resolution.ts
fn resolution_target(key: &str) -> Option<&str> {
    if key.starts_with("*/") || key.starts_with("**/") {
        return None;
    }
    // The grammar tries a parent specifier first and falls back to the target alone.
    if let Some((_, rest)) = specifier(key)
        && let Some(rest) = rest.strip_prefix('/')
        && let Some((target, "")) = specifier(rest)
    {
        return Some(target);
    }
    match specifier(key) {
        Some((target, "")) => Some(target),
        _ => None,
    }
}

/// The package name a specifier at the start of `text` names, and the text after the specifier.
///
/// Like the grammar's ordered choices, a description is taken whenever one follows the name.
fn specifier(text: &str) -> Option<(&str, &str)> {
    let (name, rest) = full_name(text)?;
    let description = rest
        .strip_prefix('@')
        .map(|after| after.find('/').unwrap_or(after.len()))
        .filter(|length| *length > 0);
    match description {
        Some(length) => Some((name, rest.get(1 + length..)?)),
        None => Some((name, rest)),
    }
}

/// The package name at the start of `text`, `@scope/name` or a bare name, and the text after it.
fn full_name(text: &str) -> Option<(&str, &str)> {
    let ident_end = |from: &str| from.find(['/', '@']).unwrap_or(from.len());
    let length = if let Some(scoped) = text.strip_prefix('@') {
        let scope = ident_end(scoped);
        let after_scope = scoped.get(scope..)?.strip_prefix('/')?;
        let name = ident_end(after_scope);
        if scope == 0 || name == 0 {
            return None;
        }
        1 + scope + 1 + name
    } else {
        let name = ident_end(text);
        if name == 0 {
            return None;
        }
        name
    };
    Some((text.get(..length)?, text.get(length..)?))
}

#[cfg(test)]
mod tests {
    use super::resolution_target;

    /// Each key Yarn's grammar reads yields the package it targets, a parent qualifier and a range
    /// aside, and every key Yarn refuses yields none.
    #[test]
    fn resolution_keys_target_their_last_package() {
        // Keys Yarn reads
        for (key, target) in [
            ("chalk", "chalk"),
            ("chalk@^4.1.0", "chalk"),
            ("a/chalk", "chalk"),
            ("a@1.0.0/chalk@npm:4.1.0", "chalk"),
            ("@scope/a/@scope/b", "@scope/b"),
            ("@s/a@1/@s/b@2", "@s/b"),
            ("a/@scope/b", "@scope/b"),
            ("@scope/b", "@scope/b"),
            ("a@1@2", "a"),
        ] {
            assert_eq!(resolution_target(key), Some(target), "{key}");
        }

        // Keys Yarn refuses
        for key in [
            "**/chalk", "*/chalk", "a//b", "a/b/c", "", "/chalk", "chalk/", "@scope", "@/b",
            "chalk@",
        ] {
            assert_eq!(resolution_target(key), None, "{key}");
        }
    }
}
