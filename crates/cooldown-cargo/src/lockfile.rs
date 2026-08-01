//! Cargo lockfile representation and package-slot projections.

use crate::version;
use cooldown_core::{CoreError, Result};
use std::collections::{BTreeMap, BTreeSet};

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

/// A `[[package]]` block's complete lockfile identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LockPackageId {
    /// The crate's package name.
    pub name: String,
    /// The resolved version.
    pub version: String,
    /// The package source, absent for path and workspace packages.
    pub source: Option<String>,
}

impl LockPackageId {
    /// Builds a complete lock package identity.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        source: Option<impl Into<String>>,
    ) -> Self {
        LockPackageId {
            name: name.into(),
            version: version.into(),
            source: source.map(Into::into),
        }
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
