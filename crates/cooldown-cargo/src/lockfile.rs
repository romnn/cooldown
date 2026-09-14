//! Cargo lockfile representation and package-slot projections.

use crate::version;
use cooldown_core::{CoreError, Result};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SourceIdentity {
    Verbatim(String),
    Git(GitSourceIdentity),
}

/// A parsed git source whose query values are compared without conflating encoded delimiters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct GitSourceIdentity {
    repository: String,
    query: Vec<(String, Option<String>)>,
    precise: Option<String>,
}

impl SourceIdentity {
    fn from_lock_source(source: &str) -> Self {
        if source.starts_with("git+") {
            SourceIdentity::Git(parse_git_source(source, true))
        } else {
            SourceIdentity::Verbatim(source.to_string())
        }
    }

    fn from_metadata_source(source: &str) -> Self {
        if source.starts_with("git+") {
            SourceIdentity::Git(parse_git_source(source, false))
        } else {
            SourceIdentity::Verbatim(source.to_string())
        }
    }
}

fn parse_git_source(source: &str, decode_lock_query: bool) -> GitSourceIdentity {
    let (without_precise, precise) = source
        .split_once('#')
        .map_or((source, None), |(base, precise)| {
            (base, Some(precise.to_string()))
        });
    let (repository, query) = without_precise
        .split_once('?')
        .map_or((without_precise, ""), |(repository, query)| {
            (repository, query)
        });
    let mut query: Vec<_> = query
        .split('&')
        .filter(|parameter| !parameter.is_empty())
        .map(|parameter| {
            let (key, value) = parameter
                .split_once('=')
                .map_or((parameter, None), |(key, value)| (key, Some(value)));
            let normalize = |value: &str| {
                if decode_lock_query {
                    decode_ascii_percent_escapes(value)
                } else {
                    value.to_string()
                }
            };
            (normalize(key), value.map(normalize))
        })
        .collect();
    query.sort();
    GitSourceIdentity {
        repository: repository.to_string(),
        query,
        precise,
    }
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
    /// Builds an identity from a `Cargo.lock` source spelling.
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        source: Option<impl Into<String>>,
    ) -> Self {
        let source = source.map(Into::into);
        let source_identity = source.as_deref().map(SourceIdentity::from_lock_source);
        LockPackageId {
            name: name.into(),
            version: version.into(),
            source,
            source_identity,
        }
    }

    /// Builds an identity from a `cargo metadata` source spelling.
    pub(crate) fn from_metadata(
        name: impl Into<String>,
        version: impl Into<String>,
        source: Option<impl Into<String>>,
    ) -> Self {
        let source = source.map(Into::into);
        let source_identity = source.as_deref().map(SourceIdentity::from_metadata_source);
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

/// A `(source, name, major)` compatibility slot: [`SlotKey`] qualified by the registry source, so
/// same-named packages from different registries never collapse into one slot.
pub(crate) type SourcedSlotKey = (String, String, String);

/// Every registry version present in each Cargo compatibility slot.
pub(crate) type LockedSlots = BTreeMap<SlotKey, BTreeSet<String>>;

/// The `Cargo.lock`'s `[[package]]` array, parsed for the before/after version diff and the
/// edge-binding policies.
/// Only the fields those need are read; Cargo owns the canonical format.
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
    /// The source URL.
    /// Absent for path/workspace members; present for registry and git crates.
    /// Only registry crates have a comparable, fetchable version, so the version diff keeps only
    /// those.
    #[serde(default)]
    pub(crate) source: Option<String>,
    /// The package's resolved dependency entries — `"name"`, `"name x.y.z"` when the lock holds
    /// several versions of the name, or `"name x.y.z (source)"` when several sources coexist.
    /// The version-qualified form is an edge *binding* the edge-policy module inspects and may
    /// rewrite.
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
    /// only source kind whose version the cooldown diff can move and compare.
    /// Git and path/workspace sources are excluded.
    fn is_registry(&self) -> bool {
        self.source
            .as_deref()
            .is_some_and(|source| source.starts_with("registry+"))
    }

    fn is_crates_io(&self) -> bool {
        self.source.as_deref() == Some(CRATES_IO_SOURCE)
    }
}

/// A source spelling without its `#commit` fragment — the form a dependency entry uses for a
/// git package whose block spells the precise revision.
fn without_precise(source: &str) -> &str {
    source.split_once('#').map_or(source, |(base, _)| base)
}

/// The blocks each walk of [`CargoLock::reachability`] visits, by index into the lock.
struct LockReachability {
    authored: HashSet<usize>,
    generated: HashSet<usize>,
}

impl CargoLock {
    /// Parses the subset of `Cargo.lock` used by the adapter.
    pub(crate) fn parse(content: &str) -> Result<Self> {
        toml::from_str(content)
            .map_err(|err| CoreError::LockUnreadable(format!("Cargo.lock: {err}")))
    }

    /// Whether the lock carries the crates.io package `name` at exactly `version`.
    pub(crate) fn has_crates_io_package(&self, name: &str, version: &str) -> bool {
        self.package.iter().any(|package| {
            package.name == name
                && package.version.as_deref() == Some(version)
                && package.is_crates_io()
        })
    }

    /// Returns every registry version grouped by Cargo compatibility slot.
    pub(crate) fn locked_slots(&self) -> LockedSlots {
        self.matching_slots(LockPackage::is_registry)
    }

    /// Returns every crates.io version grouped by Cargo compatibility slot.
    pub(crate) fn crates_io_locked_slots(&self) -> LockedSlots {
        self.matching_slots(LockPackage::is_crates_io)
    }

    /// Returns the highest crates.io version in each Cargo compatibility slot.
    pub(crate) fn crates_io_locked_versions(&self) -> BTreeMap<SlotKey, String> {
        highest_versions(self.crates_io_locked_slots())
    }

    /// Returns the highest registry version in each per-source Cargo compatibility slot.
    pub(crate) fn locked_versions_by_source(&self) -> BTreeMap<SourcedSlotKey, String> {
        let mut slots: BTreeMap<SourcedSlotKey, String> = BTreeMap::new();
        for package in &self.package {
            let (Some(version), Some(source), true) = (
                package.version.as_deref(),
                package.source.as_deref(),
                package.is_registry(),
            ) else {
                continue;
            };
            let key = (
                source.to_string(),
                package.name.clone(),
                version::major_key(version).0,
            );
            slots
                .entry(key)
                .and_modify(|highest| {
                    if version::compare(version, highest).is_gt() {
                        *highest = version.to_string();
                    }
                })
                .or_insert_with(|| version.to_string());
        }
        slots
    }

    /// The `(name, version)` crates.io packages only the `generated` workspace members reach
    /// along the lock's own dependency lists — the lock-text twin of
    /// [`ResolvedGraph::nodes_reached_only_via_generated`](crate::cargocmd::ResolvedGraph::nodes_reached_only_via_generated),
    /// for judging a mutation against the journaled lock it started from without a graph read.
    /// The walks start from the `authored` and `generated` member identities the graph
    /// established (a source-less block is not necessarily a member: an external path package
    /// is one too, and is only ever an intermediate node here).
    pub(crate) fn reached_only_via(
        &self,
        authored: &[LockPackageId],
        generated: &[LockPackageId],
    ) -> BTreeSet<(String, String)> {
        let walk = self.reachability(authored, generated);
        self.package
            .iter()
            .enumerate()
            .filter(|(index, package)| {
                package.is_crates_io()
                    && !walk.authored.contains(index)
                    && walk.generated.contains(index)
            })
            .filter_map(|(_, package)| {
                package
                    .version
                    .clone()
                    .map(|version| (package.name.clone(), version))
            })
            .collect()
    }

    /// The crates.io versions the authored members reach each crate at, keyed by name — the
    /// lines a projection is meant to mirror.
    pub(crate) fn authored_crates_io_versions(
        &self,
        authored: &[LockPackageId],
        generated: &[LockPackageId],
    ) -> BTreeMap<String, Vec<String>> {
        let walk = self.reachability(authored, generated);
        let mut versions: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (index, package) in self.package.iter().enumerate() {
            if package.is_crates_io()
                && walk.authored.contains(&index)
                && let Some(version) = &package.version
            {
                versions
                    .entry(package.name.clone())
                    .or_default()
                    .push(version.clone());
            }
        }
        versions
    }

    /// Walks the lock's dependency lists twice: from the authored members without stepping
    /// through a generated one, and from the generated members alone.
    ///
    /// A dependency entry is `"name"` when the name is unique in the lock, `"name x.y.z"` when
    /// several versions coexist, and `"name x.y.z (source)"` when several sources do — with a
    /// git source spelled *without* the `#commit` the package block carries, so sources are
    /// compared with that fragment stripped.
    fn reachability(
        &self,
        authored: &[LockPackageId],
        generated: &[LockPackageId],
    ) -> LockReachability {
        let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        for (index, package) in self.package.iter().enumerate() {
            by_name
                .entry(package.name.as_str())
                .or_default()
                .push(index);
        }
        let resolve = |entry: &str| -> Option<usize> {
            let (name, remainder) = entry.split_once(' ').unwrap_or((entry, ""));
            let candidates = by_name.get(name)?;
            if remainder.is_empty() {
                return match candidates.as_slice() {
                    [only] => Some(*only),
                    _ => None,
                };
            }
            let version = remainder.split(' ').next().unwrap_or(remainder);
            let source = remainder
                .split_once(" (")
                .and_then(|(_, source)| source.strip_suffix(')'))
                .map(without_precise);
            candidates.iter().copied().find(|&index| {
                self.package.get(index).is_some_and(|package| {
                    package.version.as_deref() == Some(version)
                        && source.is_none_or(|source| {
                            package.source.as_deref().map(without_precise) == Some(source)
                        })
                })
            })
        };
        let members_of = |identities: &[LockPackageId]| -> Vec<usize> {
            self.package
                .iter()
                .enumerate()
                .filter(|(_, package)| {
                    package.source.is_none()
                        && identities.iter().any(|id| {
                            id.name == package.name
                                && Some(id.version.as_str()) == package.version.as_deref()
                        })
                })
                .map(|(index, _)| index)
                .collect()
        };
        let generated_indices: HashSet<usize> = members_of(generated).into_iter().collect();
        let walk = |from: Vec<usize>, skip_generated: bool| -> HashSet<usize> {
            let mut seen = HashSet::new();
            let mut queue = from;
            while let Some(index) = queue.pop() {
                let Some(package) = self.package.get(index) else {
                    continue;
                };
                if !seen.insert(index) || (skip_generated && generated_indices.contains(&index)) {
                    continue;
                }
                queue.extend(
                    package
                        .dependencies
                        .iter()
                        .filter_map(|entry| resolve(entry)),
                );
            }
            seen
        };
        LockReachability {
            authored: walk(members_of(authored), true),
            generated: walk(generated_indices.iter().copied().collect(), false),
        }
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

/// One planned node move for [`rewrite_planned_nodes`]: the crates.io package `name` currently
/// locked at `from`, to be seeded at `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedNodeMove {
    pub(crate) name: String,
    pub(crate) from: String,
    pub(crate) to: String,
}

/// Rewrites each moved node's `[[package]]` block in the raw lock text so a single resolver
/// reconcile can land a co-moving family atomically: the `version` field becomes the target, and
/// the now-wrong `checksum` and stale `dependencies` array are dropped for the resolver to refill
/// from the target's real manifest. Every other byte is preserved.
///
/// Only crates.io blocks are touched, matched by exact `(name, from)`. Returns `None` — atomicity
/// abstains — when any move's block is missing, so a half-rewritten seed never reaches the
/// resolver.
pub(crate) fn rewrite_planned_nodes(lock_text: &str, moves: &[PlannedNodeMove]) -> Option<String> {
    // Parse once to locate each move's block by identity; the surgery itself stays line-based so
    // untouched blocks survive byte-identical.
    let mut remaining: Vec<&PlannedNodeMove> = moves.iter().collect();
    let mut output = String::with_capacity(lock_text.len());
    let mut lines = lock_text.lines().peekable();
    while let Some(line) = lines.next() {
        output.push_str(line);
        output.push('\n');
        if line.trim() != "[[package]]" {
            continue;
        }
        // Collect the block up to the next section header or EOF and decide whether it is a
        // moved node before emitting it. Any `[`-led line ends the block — not just the next
        // `[[package]]`: cargo serializes `[[patch.unused]]` entries (which carry their own
        // `version = "…"` fields) after the last package, and absorbing one into that package's
        // block would let the rewrite below corrupt it.
        let mut block: Vec<&str> = Vec::new();
        while let Some(&next) = lines.peek() {
            if next.trim_start().starts_with('[') {
                break;
            }
            block.push(next);
            lines.next();
        }
        let field = |key: &str| {
            block.iter().find_map(|entry| {
                entry
                    .strip_prefix(&format!("{key} = \""))
                    .and_then(|rest| rest.strip_suffix('"'))
            })
        };
        let matched = field("name")
            .zip(field("version"))
            .and_then(|(name, version)| {
                (field("source") == Some(CRATES_IO_SOURCE))
                    .then(|| {
                        remaining
                            .iter()
                            .position(|planned| planned.name == name && planned.from == version)
                    })
                    .flatten()
            });
        let Some(index) = matched else {
            for entry in block {
                output.push_str(entry);
                output.push('\n');
            }
            continue;
        };
        let planned = remaining.swap_remove(index);
        let mut in_dependencies = false;
        for entry in block {
            if in_dependencies {
                if entry.trim_start().starts_with(']') {
                    in_dependencies = false;
                }
                continue;
            }
            if entry.starts_with("checksum = ") || entry.trim() == "dependencies = []" {
                continue;
            }
            if entry.starts_with("dependencies = [") {
                in_dependencies = true;
                continue;
            }
            if entry.starts_with("version = \"") {
                output.push_str("version = \"");
                output.push_str(&planned.to);
                output.push_str("\"\n");
                continue;
            }
            output.push_str(entry);
            output.push('\n');
        }
    }
    remaining.is_empty().then_some(output)
}

#[cfg(test)]
mod tests {
    use super::LockPackageId;
    use super::{CargoLock, PlannedNodeMove, rewrite_planned_nodes};
    use indoc::indoc;

    /// `app → mid → dep 0.14` beside the generated hack's own `dep 0.13`, `dep 0.14`, and
    /// `orphan` entries; an external *path* namesake `hack 0.2.0` and a git `bridge` (qualified
    /// against a registry namesake, and spelled without the block's `#commit`) both still need
    /// `dep 0.13`.
    const HAKARI_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [
         "bridge 1.0.0 (git+https://example.com/bridge)",
         "hack 0.1.0",
         "hack 0.2.0",
         "mid",
        ]

        [[package]]
        name = "hack"
        version = "0.1.0"
        dependencies = [
         "dep 0.13.0",
         "dep 0.14.0",
         "orphan",
        ]

        [[package]]
        name = "hack"
        version = "0.2.0"
        dependencies = [
         "dep 0.13.0",
        ]

        [[package]]
        name = "bridge"
        version = "1.0.0"
        source = "git+https://example.com/bridge#abcdef0123456789"
        dependencies = [
         "dep 0.13.0",
        ]

        [[package]]
        name = "bridge"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "mid"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        dependencies = [
         "dep 0.14.0",
        ]

        [[package]]
        name = "dep"
        version = "0.13.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "dep"
        version = "0.14.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "orphan"
        version = "2.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
    "#};

    fn member(name: &str, version: &str) -> LockPackageId {
        LockPackageId::new(name, version, None::<&str>)
    }

    /// The lock-text reachability agrees with the graph's: only the orphan is projection-only,
    /// since the path namesake and the git bridge — authored reaches, neither a generated member
    /// — still need `dep 0.13`; both lines are authored; without a generated member nothing is
    /// projection-only. Without those two requirers the hack alone holds `dep 0.13`.
    #[test]
    fn lock_reachability_separates_projection_only_nodes() {
        let lock = CargoLock::parse(HAKARI_LOCK).expect("lock parses");
        let authored = [member("app", "0.1.0")];
        let generated = [member("hack", "0.1.0")];
        let only_via_hack: Vec<(String, String)> = lock
            .reached_only_via(&authored, &generated)
            .into_iter()
            .collect();
        assert_eq!(only_via_hack, [("orphan".to_string(), "2.0.0".to_string())]);
        let lines = lock.authored_crates_io_versions(&authored, &generated);
        assert_eq!(
            lines.get("dep").map(Vec::as_slice),
            Some(["0.13.0".to_string(), "0.14.0".to_string()].as_slice())
        );
        assert!(!lines.contains_key("orphan"));
        assert!(lock.reached_only_via(&authored, &[]).is_empty());

        let pruned = HAKARI_LOCK
            .replace(" \"bridge 1.0.0 (git+https://example.com/bridge)\",\n", "")
            .replace(" \"hack 0.2.0\",\n", "");
        let lock = CargoLock::parse(&pruned).expect("pruned lock parses");
        let only_via_hack: Vec<(String, String)> = lock
            .reached_only_via(&authored, &generated)
            .into_iter()
            .collect();
        assert_eq!(
            only_via_hack,
            [
                ("dep".to_string(), "0.13.0".to_string()),
                ("orphan".to_string(), "2.0.0".to_string()),
            ]
        );
    }

    const FAMILY_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [
         "icu_normalizer",
        ]

        [[package]]
        name = "icu_normalizer"
        version = "2.3.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "aa"
        dependencies = [
         "icu_normalizer_data",
         "icu_provider",
        ]

        [[package]]
        name = "icu_normalizer_data"
        version = "2.3.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "bb"

        [[package]]
        name = "icu_provider"
        version = "2.3.1"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "cc"
        dependencies = []
    "#};

    /// The atomic seed: every moved node's version is rewritten while its stale checksum and
    /// dependency edges are dropped for the reconcile to refill; untouched blocks (the `app`
    /// workspace member here) survive byte-identical.
    #[test]
    fn rewrite_planned_nodes_seeds_targets_and_drops_stale_fields() {
        let moves = [
            PlannedNodeMove {
                name: "icu_normalizer".to_string(),
                from: "2.3.0".to_string(),
                to: "2.2.0".to_string(),
            },
            PlannedNodeMove {
                name: "icu_normalizer_data".to_string(),
                from: "2.3.0".to_string(),
                to: "2.2.0".to_string(),
            },
            PlannedNodeMove {
                name: "icu_provider".to_string(),
                from: "2.3.1".to_string(),
                to: "2.2.0".to_string(),
            },
        ];

        let seeded = rewrite_planned_nodes(FAMILY_LOCK, &moves).expect("all moves present");

        let expected = indoc! {r#"
            version = 4

            [[package]]
            name = "app"
            version = "0.1.0"
            dependencies = [
             "icu_normalizer",
            ]

            [[package]]
            name = "icu_normalizer"
            version = "2.2.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "icu_normalizer_data"
            version = "2.2.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "icu_provider"
            version = "2.2.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#};
        assert_eq!(seeded, expected);
    }

    /// A `[[patch.unused]]` tail (cargo serializes it after the last package, with its own
    /// `version` field) must not be absorbed into the preceding package's block: when that
    /// package is a seeded move, absorption would rewrite the patch record's version too.
    #[test]
    fn rewrite_planned_nodes_leaves_trailing_sections_untouched() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "icu_provider"
            version = "2.3.1"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "cc"

            [[patch.unused]]
            name = "shadow"
            version = "9.9.9"
        "#};
        let moves = [PlannedNodeMove {
            name: "icu_provider".to_string(),
            from: "2.3.1".to_string(),
            to: "2.2.0".to_string(),
        }];

        let seeded = rewrite_planned_nodes(lock, &moves).expect("the move is present");

        let expected = indoc! {r#"
            version = 4

            [[package]]
            name = "icu_provider"
            version = "2.2.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[patch.unused]]
            name = "shadow"
            version = "9.9.9"
        "#};
        assert_eq!(seeded, expected);
    }

    /// A move whose node is absent aborts the whole seed: a partially seeded family handed to the
    /// resolver would be indistinguishable from a legitimate half-landed state.
    #[test]
    fn rewrite_planned_nodes_abstains_when_a_move_is_missing() {
        let moves = [PlannedNodeMove {
            name: "icu_normalizer".to_string(),
            from: "9.9.9".to_string(),
            to: "2.2.0".to_string(),
        }];
        assert_eq!(rewrite_planned_nodes(FAMILY_LOCK, &moves), None);
    }

    /// Only crates.io blocks are seeded; a same-name node from another source never matches.
    #[test]
    fn rewrite_planned_nodes_ignores_non_registry_blocks() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "twin"
            version = "1.0.0"
            source = "git+https://example.com/twin#abcdef"
        "#};
        let moves = [PlannedNodeMove {
            name: "twin".to_string(),
            from: "1.0.0".to_string(),
            to: "0.9.0".to_string(),
        }];
        assert_eq!(rewrite_planned_nodes(lock, &moves), None);
    }

    #[test]
    fn git_source_identity_joins_metadata_and_lock_spellings() {
        let metadata = LockPackageId::from_metadata(
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
    fn git_source_identity_decodes_only_the_lock_query_representation() {
        let metadata = LockPackageId::from_metadata(
            "ort",
            "1.0.0",
            Some("git+https://example.com/ort?branch=foo%2Fbar&rev=a%26b%3Dc#abcdef"),
        );
        let lock = LockPackageId::new(
            "ort",
            "1.0.0",
            Some("git+https://example.com/ort?branch=foo%252Fbar&rev=a%2526b%253Dc#abcdef"),
        );

        assert_eq!(metadata, lock);
        assert_ne!(
            metadata,
            LockPackageId::new(
                "ort",
                "1.0.0",
                Some("git+https://example.com/ort?branch=foo%2Fbar&rev=a%26b%3Dc#abcdef"),
            ),
            "a lock spelling is decoded exactly once"
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
