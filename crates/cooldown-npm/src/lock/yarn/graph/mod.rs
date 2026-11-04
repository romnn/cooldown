//! The flat package graph of a Yarn Classic lock, read through Yarn's own grammar.
//!
//! Each entry is keyed by every selector that resolves to it.
//! A selector names the package it installs the way Yarn decodes it: the name before its range
//! ([`normalizePattern`][pattern]), or, for an `npm:` or `yarn:` alias, the target the registry
//! resolver reads from that range ([`RegistryResolver`][registry]).
//! An alias is therefore read as its target's release, never as a package named after its whole
//! selector.
//!
//! [pattern]: https://github.com/yarnpkg/yarn/blob/v1.22.22/src/util/normalize-pattern.js
//! [registry]: https://github.com/yarnpkg/yarn/blob/v1.22.22/src/resolvers/exotics/registry-resolver.js

use super::{Field, Item, LockedEntry, Value, is_registry_tarball, locked_entry, read_items};
use crate::lock::{
    AliasOnlyVersions, LockOrigin, NameVersion, RegistryRelease, is_registry_specifier,
};
use std::collections::{HashMap, HashSet};

/// The protocols of Yarn's registry resolvers, the `registries` table of its resolver index.
const REGISTRY_PROTOCOLS: [&str; 2] = ["npm:", "yarn:"];

/// Every registry release and other resolved package the lock records, one row per package each
/// entry resolves, in file order, or `None` where Yarn's own reader would not read the lock.
///
/// A row carries the entry's `resolved` URL, its `#` checksum fragment stripped, as its origin.
/// An entry from a source of its own (`file:`, `link:`, git, a remote tarball) keeps its row,
/// since a member may own it as a copy fixed in place.
pub(in crate::lock) fn packages(content: &str) -> Option<Vec<NameVersion>> {
    let mut rows = Vec::new();
    for entry in entries(content)? {
        let origin = match &entry.resolved {
            Field::Text(url) => {
                let url = url.split('#').next().unwrap_or(url);
                LockOrigin::Url(url.to_string())
            }
            Field::Absent | Field::Malformed => LockOrigin::Unrecorded,
        };
        let mut packages: Vec<&str> = Vec::new();
        for Selector { package, .. } in &entry.selectors {
            if !packages.contains(&package.as_str()) {
                packages.push(package);
            }
        }
        rows.extend(packages.into_iter().map(|package| NameVersion {
            name: package.to_string(),
            version: entry.version.clone(),
            origin: origin.clone(),
        }));
    }
    Some(rows)
}

/// The registry releases the lock installs only through aliases
/// ([`NodeLock::alias_only_versions`](crate::lock::NodeLock::alias_only_versions)).
///
/// A release counts when some selector installs it under another name, unless a registry
/// selector of the release's own name binds it too.
/// That own-name binding is proven only by an entry that is a registry copy of the release
/// ([`ClassicEntry::proves_registry_copy`]), since a registry-looking range the root `resolutions`
/// rewrites may resolve to a source of its own.
/// A lock Yarn's own reader would not read has none.
pub(in crate::lock) fn alias_only_versions(content: &str) -> AliasOnlyVersions {
    let mut alias_targets = HashSet::new();
    let mut registry_copies = HashSet::new();
    for entry in entries(content).unwrap_or_default() {
        for selector in entry.selectors.iter().filter(|selector| selector.registry) {
            let release = RegistryRelease {
                name: selector.package.clone(),
                version: entry.version.clone(),
            };
            if selector.aliased {
                alias_targets.insert(release);
            } else if entry.proves_registry_copy(&selector.package) {
                registry_copies.insert(release);
            }
        }
    }
    alias_targets.retain(|release| !registry_copies.contains(release));
    AliasOnlyVersions {
        releases: alias_targets,
    }
}

/// One resolved entry of the lock.
struct ClassicEntry {
    /// Each selector the entry is keyed by.
    selectors: Vec<Selector>,
    /// The resolved `version`.
    version: String,
    /// The `resolved` URL.
    resolved: Field,
    /// The recorded package `name`.
    name: Field,
}

impl ClassicEntry {
    /// Whether the entry is a registry copy of `package` at its version: its `resolved` URL is
    /// absent or a registry tarball of that release, and its fields hold nothing Yarn never
    /// writes.
    fn proves_registry_copy(&self, package: &str) -> bool {
        let origin = match &self.resolved {
            Field::Absent => true,
            Field::Text(url) => is_registry_tarball(package, &self.version, url),
            Field::Malformed => false,
        };
        origin && self.name != Field::Malformed
    }
}

/// Every entry of the lock that records a string `version` and keeps a selector, or `None` where
/// Yarn's own reader would not read the lock.
///
/// Yarn's reader assigns an object's keys in turn, so a selector listed again by a later entry
/// belongs to that entry alone, and an entry left with no selector is no entry at all.
fn entries(content: &str) -> Option<Vec<ClassicEntry>> {
    let items = read_items(content)?;
    let mut owners: HashMap<&str, usize> = HashMap::new();
    for (index, Item { keys, .. }) in items.iter().enumerate() {
        for key in keys {
            owners.insert(key, index);
        }
    }
    let entries = items
        .iter()
        .enumerate()
        .filter_map(|(index, Item { keys, value })| {
            let Value::Object(fields) = value else {
                return None;
            };
            let LockedEntry {
                version,
                resolved,
                name,
            } = locked_entry(fields);
            let Field::Text(version) = version else {
                return None;
            };
            let recorded = match &name {
                Field::Text(name) => Some(name.as_str()),
                Field::Absent | Field::Malformed => None,
            };
            let selectors: Vec<Selector> = keys
                .iter()
                .filter(|key| owners.get(key.as_str()) == Some(&index))
                .filter_map(|key| Selector::decode(key, recorded))
                .collect();
            if selectors.is_empty() {
                return None;
            }
            Some(ClassicEntry {
                selectors,
                version,
                resolved,
                name,
            })
        })
        .collect();
    Some(entries)
}

/// The package one selector of an entry installs.
struct Selector {
    /// The package the selector resolves.
    package: String,
    /// Whether the selector installs the package under another name: an alias of it, or an entry
    /// whose recorded `name` differs from the selector's.
    aliased: bool,
    /// Whether the selector asks the registry for a release ([`is_registry_specifier`], or one of
    /// Yarn's registry protocols).
    registry: bool,
}

impl Selector {
    /// Decodes `key` of an entry whose recorded `name`, if any, is `recorded`, or `None` for a key
    /// that names no package.
    ///
    /// An alias names its target whatever the entry records, since Yarn records the name it
    /// installs the release under there.
    fn decode(key: &str, recorded: Option<&str>) -> Option<Self> {
        let Pattern { name, range } = Pattern::split(key);
        if let Some(target) = registry_target(range) {
            return (!target.is_empty()).then(|| Selector {
                package: target.to_string(),
                aliased: target != name,
                registry: true,
            });
        }
        let package = recorded.unwrap_or(name);
        (!package.is_empty()).then(|| Selector {
            package: package.to_string(),
            aliased: package != name,
            registry: is_registry_specifier(range),
        })
    }
}

/// A selector split into its name and range as Yarn's `normalizePattern` splits it.
struct Pattern<'a> {
    name: &'a str,
    range: &'a str,
}

impl<'a> Pattern<'a> {
    /// Splits `pattern` at the first `@` after a scope's leading one.
    ///
    /// A pattern with no such `@` has the range `latest`, and one ending in it the range `*`.
    fn split(pattern: &'a str) -> Self {
        let scope = usize::from(pattern.starts_with('@'));
        let at = pattern
            .get(scope..)
            .and_then(|body| body.find('@'))
            .map(|at| at + scope);
        match at.and_then(|at| Some((pattern.get(..at)?, pattern.get(at + 1..)?))) {
            Some((name, "")) => Pattern { name, range: "*" },
            Some((name, range)) => Pattern { name, range },
            None => Pattern {
                name: pattern,
                range: "latest",
            },
        }
    }
}

/// The package a registry-protocol range names, or `None` for any other range.
///
/// Yarn's registry resolver matches the range against `^(\S+):(@?.*?)(@(.*?)|)$`.
/// The protocol run therefore ends at the last `:` before any whitespace, and the package is an
/// optional leading `@` and everything up to the next `@`.
fn registry_target(range: &str) -> Option<&str> {
    if !REGISTRY_PROTOCOLS
        .iter()
        .any(|protocol| range.starts_with(protocol))
    {
        return None;
    }
    let run = range
        .find(char::is_whitespace)
        .map_or(range, |end| range.get(..end).unwrap_or(range));
    let colon = run.rfind(':')?;
    let reference = range.get(colon + 1..)?;
    let scope = usize::from(reference.starts_with('@'));
    let end = reference
        .get(scope..)
        .and_then(|body| body.find('@'))
        .map_or(reference.len(), |at| at + scope);
    reference.get(..end)
}

#[cfg(test)]
mod tests;
