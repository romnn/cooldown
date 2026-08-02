//! Parses one `Cargo.lock` into its edge-binding view: which concrete coexisting version every
//! dependency entry is bound to, plus the reference structure the safety guards and the
//! observation diff need.

use super::{LockPackageId, PackageKey};
use crate::lockfile::CargoLock;
use crate::version;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One unambiguous edge binding of a lock, as [`LockEdgeView::bindings`] yields it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Binding<'a> {
    /// The dependent package whose edge this is.
    pub(crate) dependent: &'a LockPackageId,
    /// The depended-on crate name.
    pub(crate) dependency: &'a str,
    /// The version the edge is bound to.
    pub(crate) bound: &'a str,
}

/// The version component of a qualified entry remainder (`"0.8.2 (registry+…)"` → `"0.8.2"`).
pub(super) fn remainder_version(remainder: &str) -> &str {
    remainder.split(' ').next().unwrap_or(remainder)
}

fn remainder_source(remainder: &str) -> Option<&str> {
    remainder
        .split_once(" (")
        .and_then(|(_, source)| source.strip_suffix(')'))
}

/// The edge bindings of one parsed lock, plus the reference structure the safety guards need.
pub(crate) struct LockEdgeView {
    /// The unambiguous version-qualified bindings: `(dependent, dependency name)` → bound version.
    /// Entries carrying a `(source)` suffix, dependents holding *several* qualified entries of one
    /// dependency name (renamed multi-version deps — the entry↔requirement mapping is ambiguous)
    /// are excluded; the policies never rewrite those (the observation diff still sees them via
    /// [`qualified`](Self::qualified)).
    pub(super) bindings: BTreeMap<(LockPackageId, String), String>,
    /// EVERY qualified entry per `(block, dependency name)`, keyed by the dependent's full block
    /// identity and holding the entry remainder verbatim (`"0.8.2"`, or `"0.8.2 (registry+…)"`
    /// when the entry needs a source suffix to disambiguate), each list sorted by bound version.
    /// The observation diff ([`binding_changes`](super::binding_changes)) reads this, so a rebind
    /// among entries too ambiguous to correct — renamed multi-version deps, source-suffixed
    /// entries — is still visible per block.
    pub(super) qualified: BTreeMap<(LockPackageId, String), Vec<String>>,
    /// Why a qualified binding cannot be mapped safely to one declared requirement and rewritten.
    unaddressable: BTreeMap<(LockPackageId, String), String>,
    /// Versions present per crate name, restricted to crates.io packages — the only versions a
    /// rewrite may bind to.
    pub(super) crates_io_versions: BTreeMap<String, BTreeSet<String>>,
    /// Sources present for each `(name, version)` package identity.
    package_sources: BTreeMap<PackageKey, BTreeSet<Option<String>>>,
    /// Every source-bearing package identity present in the lock.
    packages: BTreeSet<LockPackageId>,
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
/// crate name (all sources and crates.io-only), plus the sources behind each package identity.
struct PackageSurvey<'a> {
    crates_io_versions: BTreeMap<String, BTreeSet<String>>,
    /// Version list per name across ALL sources, to resolve unqualified entries: cargo only
    /// writes a bare `"name"` entry when the lock holds exactly one package of that name.
    versions_by_name: BTreeMap<&'a str, Vec<&'a str>>,
    package_sources: BTreeMap<PackageKey, BTreeSet<Option<String>>>,
    packages: BTreeSet<LockPackageId>,
}

fn survey_packages(lock: &CargoLock) -> PackageSurvey<'_> {
    let mut crates_io_versions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut versions_by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut package_sources: BTreeMap<PackageKey, BTreeSet<Option<String>>> = BTreeMap::new();
    let mut packages = BTreeSet::new();
    for package in &lock.package {
        let Some(package_version) = package.version.as_deref() else {
            continue;
        };
        versions_by_name
            .entry(package.name.as_str())
            .or_default()
            .push(package_version);
        package_sources
            .entry(PackageKey::new(&*package.name, package_version))
            .or_default()
            .insert(package.source.clone());
        packages.insert(LockPackageId::new(
            &package.name,
            package_version,
            package.source.as_deref(),
        ));
        if package.source.as_deref() == Some(crate::cargocmd::CRATES_IO_SOURCE) {
            crates_io_versions
                .entry(package.name.clone())
                .or_default()
                .insert(package_version.to_string());
        }
    }
    PackageSurvey {
        crates_io_versions,
        versions_by_name,
        package_sources,
        packages,
    }
}

impl LockEdgeView {
    pub(crate) fn from_lock(lock: &CargoLock) -> Self {
        let PackageSurvey {
            crates_io_versions,
            versions_by_name,
            package_sources,
            packages,
        } = survey_packages(lock);

        let mut bindings: BTreeMap<(LockPackageId, String), String> = BTreeMap::new();
        let mut qualified: BTreeMap<(LockPackageId, String), Vec<String>> = BTreeMap::new();
        let mut unaddressable: BTreeMap<(LockPackageId, String), String> = BTreeMap::new();
        let mut refcounts: HashMap<PackageKey, usize> = HashMap::new();
        for package in &lock.package {
            let Some(dependent) = package.id() else {
                continue;
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
                                .entry((dependent.clone(), parsed.name.to_string()))
                                .or_default()
                                .push(remainder.to_string());
                        }
                        if parsed.has_source {
                            // A source-suffixed entry disambiguates same-name-same-version packages
                            // from different registries; rewriting it could cross sources, so the
                            // whole (dependent, name) pair is off-limits.
                            unaddressable.insert(
                                (dependent.clone(), parsed.name.to_string()),
                                "the dependency entry carries a source suffix, so its requirement cannot be identified safely"
                                    .to_string(),
                            );
                            continue;
                        }
                        let key = (dependent.clone(), parsed.name.to_string());
                        if bindings.insert(key.clone(), bound.to_string()).is_some() {
                            // Two qualified entries of one name under one dependent (renamed deps
                            // at two versions): which entry belongs to which requirement is
                            // unknowable from the lock, so the pair is untouchable.
                            unaddressable.insert(
                                key,
                                "several version-qualified entries share the dependency name, so their requirements cannot be identified safely"
                                    .to_string(),
                            );
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
        for key in unaddressable.keys() {
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
            unaddressable,
            crates_io_versions,
            package_sources,
            packages,
            refcounts,
        }
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
    pub(crate) fn binding(&self, dependent: &LockPackageId, dependency: &str) -> Option<&str> {
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

    pub(crate) fn unaddressable_reason(
        &self,
        dependent: &LockPackageId,
        dependency: &str,
    ) -> Option<&str> {
        self.unaddressable
            .get(&(dependent.clone(), dependency.to_string()))
            .map(String::as_str)
    }

    pub(crate) fn dependency_target(
        &self,
        dependency: &str,
        remainder: &str,
    ) -> Option<LockPackageId> {
        let version = remainder_version(remainder);
        if let Some(source) = remainder_source(remainder) {
            return Some(LockPackageId::new(dependency, version, Some(source)));
        }
        let key = PackageKey::new(dependency, version);
        let sources = self.package_sources.get(&key)?;
        if sources.len() != 1 {
            return None;
        }
        let source = sources.iter().next()?.as_deref();
        Some(LockPackageId::new(dependency, version, source))
    }

    pub(crate) fn dependency_source(&self, dependency: &str, remainder: &str) -> Option<String> {
        self.dependency_target(dependency, remainder)
            .and_then(|target| target.source().map(str::to_string))
    }

    pub(crate) fn contains_package(&self, package: &LockPackageId) -> bool {
        self.packages.contains(package)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CHURNED_LOCK, key, package_key, path_key, view};
    use super::LockPackageId;
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
        assert_eq!(view.refcounts.get(&package_key("itoa", "1.0.11")), Some(&1));
        // `uuid 0.8.2` is referenced by both `app` (renamed dual dep) and `diesel`.
        assert_eq!(view.refcounts.get(&package_key("uuid", "0.8.2")), Some(&2));
        assert_eq!(view.refcounts.get(&package_key("uuid", "1.24.0")), Some(&1));
    }

    #[test]
    fn a_dependent_with_two_versions_of_one_name_is_ambiguous() {
        let view = view(CHURNED_LOCK);
        // `app` holds `uuid 0.8.2` AND `uuid 1.24.0` (renamed deps): the pair is untouchable.
        assert_eq!(view.binding(&path_key("app", "0.1.0"), "uuid"), None);
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
        assert_eq!(view.binding(&path_key("app", "0.1.0"), "foo"), None);
        // The reference is still counted for the orphan guard.
        assert_eq!(view.refcounts.get(&package_key("foo", "1.0.0")), Some(&1));
        // The observation multiset keeps the source suffix verbatim.
        let block = path_key("app", "0.1.0");
        assert_eq!(
            view.qualified.get(&(block, "foo".to_string())),
            Some(&vec![
                "1.0.0 (registry+https://other.example/index)".to_string()
            ])
        );
    }

    /// Twin blocks sharing a `(name, version)` identity keep distinct bindings because their full
    /// lock identities include source.
    #[test]
    fn source_distinct_twins_remain_independently_addressable() {
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
        assert_eq!(twins.binding(&key("twin", "1.0.0"), "dep"), Some("1.0.0"));
        assert_eq!(
            twins.binding(
                &LockPackageId::new("twin", "1.0.0", Some("git+https://example.com/twin#abcdef"),),
                "dep",
            ),
            Some("2.0.0")
        );
    }
}
