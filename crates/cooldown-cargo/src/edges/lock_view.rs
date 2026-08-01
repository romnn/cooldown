//! Parses one `Cargo.lock` into its edge-binding view: which concrete coexisting version every
//! dependency entry is bound to, plus the reference structure the safety guards and the
//! observation diff need.

use super::PackageKey;
use crate::tool::CargoLock;
use crate::version;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A `[[package]]` block's full identity — `source` absent for workspace/path packages. Unlike
/// [`PackageKey`] this distinguishes same-name-same-version blocks resolved from two sources, so
/// per-block observation never merges twin blocks the way the surgery-facing key would.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BlockKey {
    /// The crate's package name.
    pub(super) name: String,
    /// The resolved version.
    pub(super) version: String,
    /// The block's `source` line, verbatim.
    pub(super) source: Option<String>,
}

/// One unambiguous edge binding of a lock, as [`LockEdgeView::bindings`] yields it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding<'a> {
    /// The dependent package whose edge this is.
    pub(crate) dependent: &'a PackageKey,
    /// The depended-on crate name.
    pub(crate) dependency: &'a str,
    /// The version the edge is bound to.
    pub(crate) bound: &'a str,
}

/// One edge of a duplicate-identity dependent, as [`LockEdgeView::duplicate_identity_edges`]
/// yields it: the binding is real but unaddressable by block surgery.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DuplicateIdentityEdge {
    /// The dependent identity shared by more than one `[[package]]` block.
    pub(crate) dependent: PackageKey,
    /// The depended-on crate name.
    pub(crate) dependency: String,
    /// The version this block's edge is bound to.
    pub(crate) bound: String,
}

/// The version component of a qualified entry remainder (`"0.8.2 (registry+…)"` → `"0.8.2"`).
pub(super) fn remainder_version(remainder: &str) -> &str {
    remainder.split(' ').next().unwrap_or(remainder)
}

/// The edge bindings of one parsed lock, plus the reference structure the safety guards need.
pub(crate) struct LockEdgeView {
    /// The unambiguous version-qualified bindings: `(dependent, dependency name)` → bound version.
    /// Entries carrying a `(source)` suffix, dependents holding *several* qualified entries of one
    /// dependency name (renamed multi-version deps — the entry↔requirement mapping is ambiguous),
    /// and dependents whose own `(name, version)` identity is not unique in the lock are excluded;
    /// the policies never rewrite those (the observation diff still sees them via
    /// [`qualified`](Self::qualified)).
    pub(super) bindings: BTreeMap<(PackageKey, String), String>,
    /// EVERY qualified entry per `(block, dependency name)`, keyed by the dependent's full block
    /// identity and holding the entry remainder verbatim (`"0.8.2"`, or `"0.8.2 (registry+…)"`
    /// when the entry needs a source suffix to disambiguate), each list sorted by bound version.
    /// The observation diff ([`binding_changes`](super::binding_changes)) reads this, so a rebind
    /// among entries too ambiguous to correct — renamed multi-version deps, source-suffixed
    /// entries, twin blocks sharing one `(name, version)` identity — is still visible per block.
    pub(super) qualified: BTreeMap<(BlockKey, String), Vec<String>>,
    /// `(name, version)` identities held by more than one `[[package]]` block (the same
    /// name+version resolved from two sources, e.g. a git fork beside the crates.io release).
    /// Block-level surgery cannot address one of them unambiguously, so their edges are
    /// observation-only.
    pub(super) duplicate_identities: BTreeSet<PackageKey>,
    /// Versions present per crate name, restricted to crates.io packages — the only versions a
    /// rewrite may bind to.
    pub(super) crates_io_versions: BTreeMap<String, BTreeSet<String>>,
    /// Versions present per crate name across ALL sources. The observation diff uses this to tell
    /// a genuine rebinding (both endpoint versions locked on both sides) from an edge merely
    /// *following* a slot-level version change the report already carries as an applied row.
    pub(super) versions: BTreeMap<String, BTreeSet<String>>,
    /// How many dependency entries reference each locked `(name, version)`, counting every entry
    /// form (qualified, source-suffixed, and unqualified resolved via the single package of that
    /// name). The orphan guard keeps rewrites from dropping a still-locked version's last
    /// reference — `cargo metadata --locked` rejects an orphaned entry. Same-identity duplicates
    /// merge their counts, which can only over-count; the post-surgery `--locked` verification is
    /// the backstop for that rarity.
    pub(super) refcounts: HashMap<PackageKey, usize>,
}

/// A parsed dependency entry: `"name"`, `"name x.y.z"`, or `"name x.y.z (source)"`.
struct DependencyEntry<'a> {
    name: &'a str,
    version: Option<&'a str>,
    has_source: bool,
}

impl<'a> DependencyEntry<'a> {
    /// Everything after the name — the version plus optional source suffix, verbatim.
    fn remainder(&self, entry: &'a str) -> Option<&'a str> {
        self.version.map(|_| &entry[self.name.len() + 1..])
    }
}

fn parse_entry(entry: &str) -> DependencyEntry<'_> {
    let mut fields = entry.split(' ');
    let name = fields.next().unwrap_or_default();
    let version = fields.next();
    DependencyEntry {
        name,
        version,
        has_source: fields.next().is_some_and(|rest| rest.starts_with('(')),
    }
}

/// The package-level survey of one lock, built before the entry pass: which versions exist per
/// crate name (all sources, and crates.io-only), and which `(name, version)` identities appear in
/// more than one `[[package]]` block.
struct PackageSurvey<'a> {
    crates_io_versions: BTreeMap<String, BTreeSet<String>>,
    /// Version list per name across ALL sources, to resolve unqualified entries: cargo only
    /// writes a bare `"name"` entry when the lock holds exactly one package of that name.
    versions_by_name: BTreeMap<&'a str, Vec<&'a str>>,
    duplicate_identities: BTreeSet<PackageKey>,
    versions: BTreeMap<String, BTreeSet<String>>,
}

fn survey_packages(lock: &CargoLock) -> PackageSurvey<'_> {
    let mut crates_io_versions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut versions_by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    // The per-identity count catches same-name-same-version blocks from two sources.
    let mut identity_counts: HashMap<PackageKey, usize> = HashMap::new();
    for package in &lock.package {
        let Some(package_version) = package.version.as_deref() else {
            continue;
        };
        versions_by_name
            .entry(package.name.as_str())
            .or_default()
            .push(package_version);
        *identity_counts
            .entry(PackageKey::new(&*package.name, package_version))
            .or_default() += 1;
        if package.source.as_deref() == Some(crate::cargocmd::CRATES_IO_SOURCE) {
            crates_io_versions
                .entry(package.name.clone())
                .or_default()
                .insert(package_version.to_string());
        }
    }
    let duplicate_identities: BTreeSet<PackageKey> = identity_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(identity, _)| identity)
        .collect();
    let versions: BTreeMap<String, BTreeSet<String>> = versions_by_name
        .iter()
        .map(|(name, list)| {
            (
                (*name).to_string(),
                list.iter().map(|version| (*version).to_string()).collect(),
            )
        })
        .collect();
    PackageSurvey {
        crates_io_versions,
        versions_by_name,
        duplicate_identities,
        versions,
    }
}

impl LockEdgeView {
    pub(crate) fn from_lock(lock: &CargoLock) -> Self {
        let PackageSurvey {
            crates_io_versions,
            versions_by_name,
            duplicate_identities,
            versions,
        } = survey_packages(lock);

        let mut bindings: BTreeMap<(PackageKey, String), String> = BTreeMap::new();
        let mut qualified: BTreeMap<(BlockKey, String), Vec<String>> = BTreeMap::new();
        let mut ambiguous: BTreeSet<(PackageKey, String)> = BTreeSet::new();
        let mut refcounts: HashMap<PackageKey, usize> = HashMap::new();
        for package in &lock.package {
            let Some(package_version) = package.version.as_deref() else {
                continue;
            };
            let dependent = PackageKey::new(&*package.name, package_version);
            let block = BlockKey {
                name: package.name.clone(),
                version: package_version.to_string(),
                source: package.source.clone(),
            };
            for entry in &package.dependencies {
                let parsed = parse_entry(entry);
                match parsed.version {
                    Some(bound) => {
                        *refcounts
                            .entry(PackageKey::new(parsed.name, bound))
                            .or_default() += 1;
                        if let Some(remainder) = parsed.remainder(entry) {
                            qualified
                                .entry((block.clone(), parsed.name.to_string()))
                                .or_default()
                                .push(remainder.to_string());
                        }
                        if parsed.has_source {
                            // A source-suffixed entry disambiguates same-name-same-version packages
                            // from different registries; rewriting it could cross sources, so the
                            // whole (dependent, name) pair is off-limits.
                            ambiguous.insert((dependent.clone(), parsed.name.to_string()));
                            continue;
                        }
                        if duplicate_identities.contains(&dependent) {
                            // The dependent's own identity names more than one block; surgery
                            // could not address the right one, so its pairs are untouchable.
                            ambiguous.insert((dependent.clone(), parsed.name.to_string()));
                            continue;
                        }
                        let key = (dependent.clone(), parsed.name.to_string());
                        if bindings.insert(key.clone(), bound.to_string()).is_some() {
                            // Two qualified entries of one name under one dependent (renamed deps
                            // at two versions): which entry belongs to which requirement is
                            // unknowable from the lock, so the pair is untouchable.
                            ambiguous.insert(key);
                        }
                    }
                    None => {
                        // An unqualified entry references the single package of that name.
                        if let Some(versions) = versions_by_name.get(parsed.name)
                            && let [only] = versions.as_slice()
                        {
                            *refcounts
                                .entry(PackageKey::new(parsed.name, *only))
                                .or_default() += 1;
                        }
                    }
                }
            }
        }
        for key in &ambiguous {
            bindings.remove(key);
        }
        for remainders in qualified.values_mut() {
            remainders.sort_by(|a, b| {
                version::compare(remainder_version(a), remainder_version(b)).then_with(|| a.cmp(b))
            });
        }
        LockEdgeView {
            bindings,
            qualified,
            duplicate_identities,
            crates_io_versions,
            versions,
            refcounts,
        }
    }

    pub(super) fn has_version(&self, name: &str, version: &str) -> bool {
        self.versions
            .get(name)
            .is_some_and(|versions| versions.contains(version))
    }

    /// The unambiguous bindings, in deterministic order.
    pub(crate) fn bindings(&self) -> impl Iterator<Item = Binding<'_>> {
        self.bindings
            .iter()
            .map(|((dependent, dependency), bound)| Binding {
                dependent,
                dependency,
                bound,
            })
    }

    /// The binding of `dependent`'s edge to `dependency`, when unambiguous.
    pub(crate) fn binding(&self, dependent: &PackageKey, dependency: &str) -> Option<&str> {
        self.bindings
            .get(&(dependent.clone(), dependency.to_string()))
            .map(String::as_str)
    }

    /// The crates.io versions of `name` present in this lock.
    pub(crate) fn crates_io_versions(&self, name: &str) -> impl Iterator<Item = &str> {
        self.crates_io_versions
            .get(name)
            .into_iter()
            .flat_map(|versions| versions.iter().map(String::as_str))
    }

    pub(crate) fn has_crates_io_version(&self, name: &str, version: &str) -> bool {
        self.crates_io_versions
            .get(name)
            .is_some_and(|versions| versions.contains(version))
    }

    /// The edges of duplicate-identity dependents whose dependency has several coexisting locked
    /// versions: bindings a corrective policy cannot address (block surgery cannot single out one
    /// of the twin blocks) but must not skip silently. Deduplicated — twins bound to the same
    /// version yield one edge.
    pub(crate) fn duplicate_identity_edges(&self) -> Vec<DuplicateIdentityEdge> {
        let mut edges = BTreeSet::new();
        for ((block, dependency), remainders) in &self.qualified {
            let identity = PackageKey::new(&*block.name, &*block.version);
            if !self.duplicate_identities.contains(&identity) {
                continue;
            }
            if self
                .versions
                .get(dependency)
                .is_none_or(|versions| versions.len() < 2)
            {
                continue;
            }
            for remainder in remainders {
                edges.insert(DuplicateIdentityEdge {
                    dependent: identity.clone(),
                    dependency: dependency.clone(),
                    bound: remainder_version(remainder).to_string(),
                });
            }
        }
        edges.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CHURNED_LOCK, key, view};
    use super::{BlockKey, DuplicateIdentityEdge};
    use indoc::indoc;

    #[test]
    fn bindings_capture_only_version_qualified_entries() {
        let view = view(CHURNED_LOCK);
        assert_eq!(
            view.binding(&key("diesel", "2.3.11"), "uuid"),
            Some("0.8.2")
        );
        // The unqualified `"itoa"` entry is not a binding (nothing to disambiguate).
        assert_eq!(view.binding(&key("diesel", "2.3.11"), "itoa"), None);
        assert_eq!(
            view.crates_io_versions("uuid").collect::<Vec<_>>(),
            vec!["0.8.2", "1.24.0"]
        );
    }

    #[test]
    fn refcounts_resolve_unqualified_entries_to_the_single_version() {
        let view = view(CHURNED_LOCK);
        // `itoa` is referenced once, via the unqualified entry.
        assert_eq!(view.refcounts.get(&key("itoa", "1.0.11")), Some(&1));
        // `uuid 0.8.2` is referenced by both `app` (renamed dual dep) and `diesel`.
        assert_eq!(view.refcounts.get(&key("uuid", "0.8.2")), Some(&2));
        assert_eq!(view.refcounts.get(&key("uuid", "1.24.0")), Some(&1));
    }

    #[test]
    fn a_dependent_with_two_versions_of_one_name_is_ambiguous() {
        let view = view(CHURNED_LOCK);
        // `app` holds `uuid 0.8.2` AND `uuid 1.24.0` (renamed deps): the pair is untouchable.
        assert_eq!(view.binding(&key("app", "0.1.0"), "uuid"), None);
    }

    #[test]
    fn source_suffixed_entries_are_ambiguous() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "app"
            version = "0.1.0"
            dependencies = [
             "foo 1.0.0 (registry+https://other.example/index)",
            ]

            [[package]]
            name = "foo"
            version = "1.0.0"
            source = "registry+https://other.example/index"
        "#};
        let view = view(lock);
        assert_eq!(view.binding(&key("app", "0.1.0"), "foo"), None);
        // The reference is still counted for the orphan guard.
        assert_eq!(view.refcounts.get(&key("foo", "1.0.0")), Some(&1));
        // The observation multiset keeps the source suffix verbatim.
        let block = BlockKey {
            name: "app".to_string(),
            version: "0.1.0".to_string(),
            source: None,
        };
        assert_eq!(
            view.qualified.get(&(block, "foo".to_string())),
            Some(&vec![
                "1.0.0 (registry+https://other.example/index)".to_string()
            ])
        );
    }

    /// Twin blocks sharing a `(name, version)` identity keep separate observation multisets — the
    /// block key includes the source — while their surgery-facing bindings stay excluded.
    #[test]
    fn duplicate_identity_edges_are_enumerated_per_block() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "dep"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "dep"
            version = "2.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "twin"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            dependencies = [
             "dep 1.0.0",
            ]

            [[package]]
            name = "twin"
            version = "1.0.0"
            source = "git+https://example.com/twin#abcdef"
            dependencies = [
             "dep 2.0.0",
            ]
        "#};
        let twins = view(lock);
        assert_eq!(twins.binding(&key("twin", "1.0.0"), "dep"), None);
        let bound_edge = |bound: &str| DuplicateIdentityEdge {
            dependent: key("twin", "1.0.0"),
            dependency: "dep".to_string(),
            bound: bound.to_string(),
        };
        assert_eq!(
            twins.duplicate_identity_edges(),
            vec![bound_edge("1.0.0"), bound_edge("2.0.0")]
        );
    }
}
