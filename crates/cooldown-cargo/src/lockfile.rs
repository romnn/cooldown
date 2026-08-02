//! Cargo lockfile representation and package-slot projections.

use crate::version;
use cooldown_core::{CoreError, Result};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

/// The `source` string Cargo records for crates.io packages.
pub(crate) const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// A source-free package identity used only by compatibility-slot and reference-count projections
/// that Cargo itself expresses by name and version.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PackageKey {
    /// The crate's package name.
    pub(crate) name: String,
    /// The resolved version.
    pub(crate) version: String,
}

impl PackageKey {
    /// Builds the identity from anything string-like, cloning borrowed inputs.
    pub(crate) fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        PackageKey {
            name: name.into(),
            version: version.into(),
        }
    }
}

/// The equality identity of a Cargo package source.
///
/// Cargo metadata can leave reserved characters literal in a git query while package IDs and
/// `Cargo.lock` percent-encode them. Git source equality decodes ASCII percent escapes so those
/// spellings join, while the verbatim source remains available for reporting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SourceIdentity(String);

impl SourceIdentity {
    fn new(source: &str) -> Self {
        if source.starts_with("git+") {
            SourceIdentity(canonical_git_source(source))
        } else {
            SourceIdentity(source.to_string())
        }
    }
}

fn canonical_git_source(source: &str) -> String {
    let Some(query_start) = source.find('?') else {
        return source.to_string();
    };
    let query_end = source[query_start..]
        .find('#')
        .map_or(source.len(), |offset| query_start + offset);
    let query = &source[query_start..query_end];
    let mut canonical = String::with_capacity(source.len());
    canonical.push_str(&source[..query_start]);
    canonical.push_str(&decode_ascii_percent_escapes(query));
    canonical.push_str(&source[query_end..]);
    canonical
}

fn decode_ascii_percent_escapes(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = bytes
            .get(index..index.saturating_add(3))
            .and_then(|escape| match escape {
                [b'%', high, low] => hex_value(*high)
                    .zip(hex_value(*low))
                    .map(|(high, low)| (high << 4) | low)
                    .filter(u8::is_ascii),
                _ => None,
            });
        if let Some(byte) = escaped {
            decoded.push(byte);
            index += 3;
        } else if let Some(byte) = bytes.get(index) {
            decoded.push(*byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| value.to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A `[[package]]` block's complete lockfile identity.
#[derive(Debug, Clone)]
pub struct LockPackageId {
    /// The crate's package name.
    pub name: String,
    /// The resolved version.
    pub version: String,
    /// The package source, absent for path and workspace packages.
    source: Option<String>,
    source_identity: Option<SourceIdentity>,
}

impl LockPackageId {
    /// Builds a complete lock package identity.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        source: Option<impl Into<String>>,
    ) -> Self {
        let source = source.map(Into::into);
        let source_identity = source.as_deref().map(SourceIdentity::new);
        LockPackageId {
            name: name.into(),
            version: version.into(),
            source,
            source_identity,
        }
    }

    /// Returns the source spelling carried by the metadata or lockfile that constructed the ID.
    #[must_use]
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }
}

impl PartialEq for LockPackageId {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.version == other.version
            && self.source_identity == other.source_identity
    }
}

impl Eq for LockPackageId {}

impl PartialOrd for LockPackageId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LockPackageId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.name
            .cmp(&other.name)
            .then_with(|| self.version.cmp(&other.version))
            .then_with(|| self.source_identity.cmp(&other.source_identity))
    }
}

impl Hash for LockPackageId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.version.hash(state);
        self.source_identity.hash(state);
    }
}

/// A `(name, major)` compatibility slot in a Cargo lockfile.
pub(crate) type SlotKey = (String, String);

/// Every registry version present in each Cargo compatibility slot.
pub(crate) type LockedSlots = BTreeMap<SlotKey, BTreeSet<String>>;

/// The `Cargo.lock`'s `[[package]]` array, parsed for the before/after version diff and the
/// edge-binding policies. Only the fields those need are read; Cargo owns the canonical format.
#[derive(serde::Deserialize)]
pub(crate) struct CargoLock {
    #[serde(default)]
    pub(crate) package: Vec<LockPackage>,
}

/// The lockfile fields needed to identify a package and inspect its resolved edges.
#[derive(serde::Deserialize)]
pub(crate) struct LockPackage {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) version: Option<String>,
    /// The source URL. Absent for path/workspace members; present for registry and git crates. Only
    /// registry crates have a comparable, fetchable version, so the version diff keeps only those.
    #[serde(default)]
    pub(crate) source: Option<String>,
    /// The package's resolved dependency entries — `"name"`, `"name x.y.z"` when the lock holds
    /// several versions of the name, or `"name x.y.z (source)"` when several sources coexist. The
    /// version-qualified form is an edge *binding* the edge-policy module inspects and may rewrite.
    #[serde(default)]
    pub(crate) dependencies: Vec<String>,
}

impl LockPackage {
    /// Returns the package's complete identity when the lock block carries a version.
    pub(crate) fn id(&self) -> Option<LockPackageId> {
        self.version
            .as_ref()
            .map(|version| LockPackageId::new(&self.name, version, self.source.as_deref()))
    }

    /// Whether this locked package came from a registry (crates.io or an alternate registry), the
    /// only source kind whose version the cooldown diff can move and compare. Git and path/workspace
    /// sources are excluded.
    fn is_registry(&self) -> bool {
        self.source
            .as_deref()
            .is_some_and(|source| source.starts_with("registry+"))
    }

    fn is_crates_io(&self) -> bool {
        self.source.as_deref() == Some(CRATES_IO_SOURCE)
    }
}

impl CargoLock {
    /// Parses the subset of `Cargo.lock` used by the adapter.
    pub(crate) fn parse(content: &str) -> Result<Self> {
        toml::from_str(content)
            .map_err(|err| CoreError::LockUnreadable(format!("Cargo.lock: {err}")))
    }

    /// Returns every registry version grouped by Cargo compatibility slot.
    pub(crate) fn locked_slots(&self) -> LockedSlots {
        self.matching_slots(LockPackage::is_registry)
    }

    /// Returns every crates.io version grouped by Cargo compatibility slot.
    pub(crate) fn crates_io_locked_slots(&self) -> LockedSlots {
        self.matching_slots(LockPackage::is_crates_io)
    }

    /// Returns the highest registry version in each Cargo compatibility slot.
    pub(crate) fn locked_versions(&self) -> BTreeMap<SlotKey, String> {
        highest_versions(self.locked_slots())
    }

    /// Returns the highest crates.io version in each Cargo compatibility slot.
    pub(crate) fn crates_io_locked_versions(&self) -> BTreeMap<SlotKey, String> {
        highest_versions(self.crates_io_locked_slots())
    }

    fn matching_slots(&self, include: impl Fn(&LockPackage) -> bool) -> LockedSlots {
        let mut slots = BTreeMap::new();
        for package in &self.package {
            let (Some(version), true) = (package.version.as_deref(), include(package)) else {
                continue;
            };
            slots
                .entry((package.name.clone(), version::major_key(version).0))
                .or_insert_with(BTreeSet::new)
                .insert(version.to_string());
        }
        slots
    }
}

fn highest_versions(slots: LockedSlots) -> BTreeMap<SlotKey, String> {
    slots
        .into_iter()
        .filter_map(|(key, versions)| {
            versions
                .into_iter()
                .max_by(|left, right| version::compare(left, right))
                .map(|version| (key, version))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::LockPackageId;

    #[test]
    fn git_source_identity_joins_metadata_and_lock_spellings() {
        let metadata = LockPackageId::new(
            "ort",
            "1.0.0",
            Some("git+https://example.com/ort?branch=chore/ort-rc-12#abcdef"),
        );
        let lock = LockPackageId::new(
            "ort",
            "1.0.0",
            Some("git+https://example.com/ort?branch=chore%2Fort-rc-12#abcdef"),
        );

        assert_eq!(metadata, lock);
        assert_eq!(
            lock.source(),
            Some("git+https://example.com/ort?branch=chore%2Fort-rc-12#abcdef")
        );
    }

    #[test]
    fn source_distinct_packages_remain_distinct() {
        let registry = LockPackageId::new(
            "twin",
            "1.0.0",
            Some("registry+https://github.com/rust-lang/crates.io-index"),
        );
        let git = LockPackageId::new("twin", "1.0.0", Some("git+https://example.com/twin#abcdef"));

        assert_ne!(registry, git);
    }
}
