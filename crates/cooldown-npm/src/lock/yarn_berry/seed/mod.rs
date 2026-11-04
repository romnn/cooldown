//! Descriptor seeding: binding the ranges a project declares to one exact registry release.
//!
//! Yarn reuses every binding its lock records ([LockfileResolver.ts][lockfile]) and resolves only
//! the descriptors the lock lacks, each to the newest release its range admits.
//! An exact landing of `chalk@4.1.1` under a declared `^4.0.0` therefore cannot come from Yarn
//! alone: a temporary exact pin makes Yarn resolve and record the release, but once the declared
//! range is restored Yarn either reuses the range's old binding, when another dependent kept it,
//! or resolves the range afresh to its maximum.
//!
//! Seeding closes that gap with an edit of the lock's keys alone.
//! Each declared descriptor (`chalk@npm:^4.0.0`) is listed in the key of the entry the temporary
//! pin resolved, and removed from every other key, an entry left without one dropped.
//! Every field of every entry stays as Yarn wrote it, so the release's metadata, checksum, and
//! dependencies are Yarn's own, and the lock-only install that follows rewrites the lock in
//! Yarn's canonical form, reusing the seeded bindings.
//!
//! An edit is accepted only when the edited lock reads back with every seeded descriptor bound to
//! the release and every other descriptor bound where it was.
//!
//! A lock-only install in a fresh cache also needs the archive of every `file:` package, which
//! Yarn packs again on each resolution, yet fetches only for an entry without a checksum.
//! [`without_local_checksums`] drops those checksums, which Yarn then recomputes from the same
//! folders.
//!
//! [lockfile]: https://github.com/yarnpkg/berry/blob/%40yarnpkg/cli/4.18.1/packages/yarnpkg-core/sources/LockfileResolver.ts

#[cfg(test)]
mod tests;

use super::{
    BerryLock, BerrySource, Descriptor, Ident, LAST_PROTOCOL_LESS_SCHEMA, NPM_PROTOCOL,
    WorkspaceEntry, combined_key_descriptors, is_semver_range, unreadable, with_default_protocol,
};
use cooldown_core::{CoreError, Result};
use std::collections::{HashMap, HashSet};

/// The longest key Yarn writes as a plain mapping key; a longer one takes YAML's explicit
/// `? key` form, which every YAML reader accepts whatever its length.
const IMPLICIT_KEY_LIMIT: usize = 1024;

/// One exact landing: the registry release of a package and the declared ranges to bind it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Seeding<'a> {
    /// The package's name, a scoped one included.
    pub(crate) name: &'a str,
    /// The release every seeded range binds.
    pub(crate) version: &'a str,
    /// The descriptor ranges as the lock spells them ([`seed_range`]).
    pub(crate) ranges: &'a [String],
}

/// The descriptor range an install declaration `specifier` resolves under, when seeding can bind
/// it to `version`, or why it cannot.
///
/// Only a same-name registry range qualifies: a semver range, alone or behind the `npm:` protocol,
/// that admits `version`.
/// A dist-tag, an alias, or any other protocol has no range seeding could prove the release
/// satisfies, and Yarn would reuse whatever binding seeding gave it.
pub(crate) fn seed_range(specifier: &str, version: &str) -> std::result::Result<String, String> {
    let range = with_default_protocol(specifier);
    let semver = range
        .strip_prefix(NPM_PROTOCOL)
        .filter(|semver| is_semver_range(semver))
        .ok_or_else(|| format!("`{specifier}` is no registry range of the package itself"))?;
    if crate::version::range_match(semver, version) != crate::version::RangeMatch::Matches {
        return Err(format!("`{specifier}` does not admit {version}"));
    }
    Ok(range)
}

/// The range of the temporary exact pin that makes Yarn resolve `version`: the version under the
/// `npm:` protocol, as Yarn normalizes a bare version.
pub(crate) fn pin_range(version: &str) -> String {
    format!("{NPM_PROTOCOL}{version}")
}

/// Why seeding `seeding` into the lock `content` would move a binding the landing does not own,
/// one reason per conflict, or none.
///
/// The landing owns the copy it moves, `seeding.name` at `from`: a seeded range that the lock binds
/// to any other locator than that copy or the target release would carry that locator's other
/// dependents along.
/// A range a member at one of the `excluded` paths records is that member's binding too, since
/// Yarn binds a descriptor once for the whole project.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] for a lock the Berry reader refuses, or a name that is
/// no package name.
pub(crate) fn conflicts(
    content: &str,
    seeding: &Seeding<'_>,
    from: &str,
    excluded: &HashSet<String>,
) -> Result<Vec<String>> {
    let lock = BerryLock::read(content)?;
    let ident = ident(seeding.name)?;
    let mut reasons = Vec::new();
    for range in seeding.ranges {
        let descriptor = Descriptor {
            ident: ident.clone(),
            range: range.clone(),
        };
        if let Some(package) = lock.package_for(&descriptor) {
            let owned = package.locator.ident == ident
                && matches!(&package.source, BerrySource::Registry { version }
                    if version == from || version == seeding.version);
            if !owned {
                reasons.push(format!(
                    "{descriptor} also binds {locator}, whose other dependents would move with it",
                    locator = package.locator
                ));
            }
        }
        for WorkspaceEntry {
            path, dependencies, ..
        } in lock.workspaces()
        {
            if !excluded.contains(path) {
                continue;
            }
            let recorded = dependencies.get(seeding.name).and_then(Option::as_deref);
            let recorded = recorded.map(|recorded| {
                if lock.schema <= LAST_PROTOCOL_LESS_SCHEMA {
                    with_default_protocol(recorded)
                } else {
                    recorded.to_string()
                }
            });
            if recorded.as_deref() == Some(range.as_str()) {
                reasons.push(format!(
                    "{descriptor} is declared by {path} too, which the run excludes, and Yarn binds \
                     one descriptor for the whole project"
                ));
            }
        }
    }
    Ok(reasons)
}

/// The lock `content` with every range of `seeding` bound to the release the temporary exact pin
/// ([`pin_range`]) resolved.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] when the lock cannot be read, the pin resolved nothing or
/// no registry release of the package at the version, or the edited lock would bind anything
/// else than intended.
pub(crate) fn seed(content: &str, seeding: &Seeding<'_>) -> Result<String> {
    let lock = BerryLock::read(content)?;
    let ident = ident(seeding.name)?;
    let pin = Descriptor {
        ident: ident.clone(),
        range: pin_range(seeding.version),
    };
    let Some(&target) = lock.index.get(&pin) else {
        return Err(unreadable(&format!(
            "the temporary pin {pin} resolved no entry to seed"
        )));
    };
    let released = lock.packages.get(target).is_some_and(|package| {
        package.locator.ident == ident
            && matches!(&package.source, BerrySource::Registry { version } if version == seeding.version)
    });
    if !released {
        return Err(unreadable(&format!(
            "the temporary pin {pin} resolved no registry release of {name} at {version}",
            name = seeding.name,
            version = seeding.version
        )));
    }
    let seeds: Vec<Descriptor> = seeding
        .ranges
        .iter()
        .map(|range| Descriptor {
            ident: ident.clone(),
            range: range.clone(),
        })
        .collect();
    let edited = rewrite_keys(content, &lock, target, &seeds)?;
    confirm(&lock, &edited, Some(target), &seeds)?;
    Ok(edited)
}

/// The lock `content` with the checksum of every `file:` package dropped, each bound where it was.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] when the lock cannot be read, or would read back
/// otherwise.
pub(crate) fn without_local_checksums(content: &str) -> Result<String> {
    let lock = BerryLock::read(content)?;
    let mut edited = String::with_capacity(content.len());
    let mut lines = content.split_inclusive('\n').peekable();
    let mut local = false;
    while let Some(line) = lines.next() {
        if let Some(key) = EntryKey::parse(line, &mut lines)? {
            local = key.text != super::METADATA_KEY
                && combined_key_descriptors(&key.text)
                    .next()
                    .map(|spelled| parsed_descriptor(&lock, spelled))
                    .transpose()?
                    .and_then(|descriptor| lock.package_for(&descriptor))
                    .is_some_and(|package| package.locator.reference.starts_with("file:"));
            edited.push_str(&key.source);
            continue;
        }
        if local && line.starts_with("  checksum:") {
            continue;
        }
        edited.push_str(line);
    }
    confirm(&lock, &edited, None, &[])?;
    Ok(edited)
}

/// The package identity `name` spells.
fn ident(name: &str) -> Result<Ident> {
    Ident::parse(name).ok_or_else(|| unreadable(&format!("`{name}` is no package name")))
}

/// `content` with each seed listed in the key of the entry at `target` and in no other key.
fn rewrite_keys(
    content: &str,
    lock: &BerryLock,
    target: usize,
    seeds: &[Descriptor],
) -> Result<String> {
    let mut edited = String::with_capacity(content.len());
    let mut lines = content.split_inclusive('\n').peekable();
    let mut seeded = false;
    while let Some(line) = lines.next() {
        let Some(key) = EntryKey::parse(line, &mut lines)? else {
            edited.push_str(line);
            continue;
        };
        if key.text == super::METADATA_KEY {
            edited.push_str(&key.source);
            continue;
        }
        let spelled: Vec<&str> = combined_key_descriptors(&key.text).collect();
        let descriptors = spelled
            .iter()
            .map(|spelled| parsed_descriptor(lock, spelled))
            .collect::<Result<Vec<_>>>()?;
        let position = descriptors
            .first()
            .and_then(|descriptor| lock.index.get(descriptor).copied())
            .ok_or_else(|| unreadable(&format!("the entry \"{}\" binds nothing", key.text)))?;
        let mut kept: Vec<String> = spelled
            .iter()
            .zip(&descriptors)
            .filter(|(_, descriptor)| !seeds.contains(descriptor))
            .map(|(spelled, _)| (*spelled).to_string())
            .collect();
        if position == target && !seeded {
            kept.extend(seeds.iter().map(ToString::to_string));
            seeded = true;
        }
        if kept.is_empty() {
            // An entry left without a descriptor is dropped together with its fields, up to the
            // next entry.
            while lines
                .peek()
                .is_some_and(|next| next.starts_with([' ', '\t']) || next.trim().is_empty())
            {
                lines.next();
            }
            continue;
        }
        if kept.len() == spelled.len() && position != target {
            edited.push_str(&key.source);
            continue;
        }
        edited.push_str(&key_lines(&kept.join(", "), key.ending)?);
    }
    if !seeded {
        return Err(unreadable(
            "the seeded entry has no key the edit could rewrite",
        ));
    }
    Ok(edited)
}

/// The descriptor `spelled` in a key of `lock`, as Yarn's reader stores it.
fn parsed_descriptor(lock: &BerryLock, spelled: &str) -> Result<Descriptor> {
    let descriptor = Descriptor::parse(spelled)
        .ok_or_else(|| unreadable(&format!("`{spelled}` is no descriptor")))?;
    Ok(if lock.schema <= LAST_PROTOCOL_LESS_SCHEMA {
        descriptor.with_protocol_restored()
    } else {
        descriptor
    })
}

/// The lines of a top-level key `text`, quoted as Yarn quotes every descriptor key, in the
/// explicit form once it is too long for a plain key.
fn key_lines(text: &str, ending: &str) -> Result<String> {
    let quoted =
        serde_json::to_string(text).map_err(|err| CoreError::Serialization(err.to_string()))?;
    Ok(if quoted.len() > IMPLICIT_KEY_LIMIT {
        format!("? {quoted}{ending}:{ending}")
    } else {
        format!("{quoted}:{ending}")
    })
}

/// A top-level key of the lock document, as written.
struct EntryKey<'a> {
    /// The decoded key.
    text: String,
    /// The lines that spell it, unchanged.
    source: String,
    /// The line ending the key's lines use.
    ending: &'a str,
}

impl<'a> EntryKey<'a> {
    /// The top-level key `line` opens, taking the `:` line of an explicit key from `rest`, or
    /// `None` for a line that opens none: an indented field, a blank line, or a comment.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockUnreadable`] for a top-level line of another shape than Yarn
    /// writes: `"key":`, `key:`, or `? "key"` followed by a `:` line.
    fn parse(
        line: &'a str,
        rest: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    ) -> Result<Option<Self>> {
        if line.starts_with([' ', '\t', '#']) || line.trim().is_empty() {
            return Ok(None);
        }
        let ending = if line.ends_with("\r\n") {
            "\r\n"
        } else if line.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let body = line.trim_end_matches(['\r', '\n']);
        let unexpected = || unreadable(&format!("the line `{body}` opens no entry Yarn writes"));
        if let Some(explicit) = body.strip_prefix("? ") {
            let colon = rest.next().ok_or_else(unexpected)?;
            if colon.trim_end_matches(['\r', '\n']) != ":" {
                return Err(unexpected());
            }
            return Ok(Some(EntryKey {
                text: decoded_key(explicit).ok_or_else(unexpected)?,
                source: format!("{line}{colon}"),
                ending,
            }));
        }
        let key = body.strip_suffix(':').ok_or_else(unexpected)?;
        Ok(Some(EntryKey {
            text: decoded_key(key).ok_or_else(unexpected)?,
            source: line.to_string(),
            ending,
        }))
    }
}

/// A key as written: a JSON-quoted string, which is how Yarn quotes one, or a plain scalar
/// without quotes, `:`, or `#`.
fn decoded_key(written: &str) -> Option<String> {
    if written.starts_with('"') {
        return serde_json::from_str(written).ok();
    }
    let plain = !written.is_empty()
        && !written.starts_with(['\'', ' '])
        && !written.contains([':', '#', '"']);
    plain.then(|| written.to_string())
}

/// Confirms that `edited` binds exactly what an edit intends: every descriptor of `lock` where it
/// was, except each seed, which binds the entry at `target`.
fn confirm(
    lock: &BerryLock,
    edited: &str,
    target: Option<usize>,
    seeds: &[Descriptor],
) -> Result<()> {
    let after = BerryLock::read(edited)?;
    let target = match target {
        Some(target) => lock
            .packages
            .get(target)
            .map(|package| Some(&package.locator))
            .ok_or_else(|| unreadable("the seeded entry is missing"))?,
        None => None,
    };
    let locator_of = |lock: &BerryLock, descriptor: &Descriptor| {
        lock.package_for(descriptor)
            .map(|package| package.locator.clone())
    };
    let mut expected: HashMap<&Descriptor, Option<_>> = lock
        .index
        .keys()
        .map(|descriptor| (descriptor, locator_of(lock, descriptor)))
        .collect();
    for seed in seeds {
        expected.insert(seed, target.cloned());
    }
    let unexpected = after
        .index
        .keys()
        .any(|descriptor| !expected.contains_key(descriptor));
    let moved = expected
        .iter()
        .any(|(descriptor, locator)| locator_of(&after, descriptor) != *locator);
    if unexpected || moved {
        return Err(unreadable(
            "the edit of the lock would change other bindings than it intends",
        ));
    }
    Ok(())
}
