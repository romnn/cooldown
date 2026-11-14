//! Per-copy accounting of a pnpm resolve: where each requirement that bound a resolved copy
//! before the resolve binds after it.
//!
//! A name can resolve to several copies, each bound by its own incoming edges, so a planned move of
//! one copy is proven only by that copy's edges, never by the name's newest version or its version
//! set.
//! The lock records edges, not which record of the settled lock continues which record of the
//! starting one, so [`CopyLedger::from_bindings`] pairs each edge by the most specific identity that
//! survives the resolve and leaves an edge it cannot pair unproven.

use crate::lock::{BindingContext, CopyBinding, NodeLock};
use cooldown_core::{Change, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// The most dependents a report detail names before it summarizes the rest.
const NAMED_DEPENDENTS: usize = 3;

/// Who holds a requirement edge, as reports name it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Dependent {
    /// A workspace importer, by its path.
    Importer(String),
    /// A resolved package, by name: its own version may move in the same resolve.
    Package(String),
}

impl Dependent {
    fn of(context: &BindingContext) -> Self {
        match context {
            BindingContext::Importer(path) => Self::Importer(path.clone()),
            BindingContext::Package { name, .. } => Self::Package(name.clone()),
        }
    }

    /// The dependent's version, which only a package has.
    fn version(context: &BindingContext) -> Option<&str> {
        match context {
            BindingContext::Importer(_) => None,
            BindingContext::Package { version, .. } => Some(version),
        }
    }
}

/// How reports name the record `context`: an importer by its path, a package by its name and
/// version.
fn label(context: &BindingContext) -> String {
    match context {
        BindingContext::Importer(path) => format!("importer `{path}`"),
        BindingContext::Package { name, version, .. } => format!("{name}@{version}"),
    }
}

/// Where one edge that bound a copy before the resolve went.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fate {
    /// The edge binds this version after the resolve, the copy's own version when it stayed.
    Bound(String),
    /// The dependent no longer requires the name.
    Dropped,
    /// The dependent's records changed in a way that pairs no old version with one new one.
    Unpaired,
}

/// One edge into a copy, judged across the resolve.
#[derive(Debug, Clone)]
struct EdgeFate {
    dependent: Dependent,
    /// The records holding the edge as reports name them: after the resolve for a bound edge,
    /// before it otherwise.
    holders: String,
    name: String,
    from: String,
    fate: Fate,
}

/// A dependent's requirement of one name whose records the resolve rewrote so that several
/// versions sit on one side, leaving no single pairing of old and new versions
/// ([`DiagnosticKind::UnpairedMove`](cooldown_core::DiagnosticKind::UnpairedMove)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnpairedRequirement {
    /// The dependent, as reports name it.
    pub(super) dependent: String,
    /// The required package.
    pub(super) name: String,
    /// The versions the dependent's unpaired records bound before the resolve.
    pub(super) before: BTreeSet<String>,
    /// The versions its unpaired records bind after it.
    pub(super) after: BTreeSet<String>,
}

/// One net move of a copy's edges: `dependents` bound `name` at `from` and now bind `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CopyMove {
    pub(super) name: String,
    pub(super) from: String,
    pub(super) to: String,
}

/// The edges of a pnpm lock before and after one resolve, paired per incoming edge.
#[derive(Debug, Default)]
pub(super) struct CopyLedger {
    fates: Vec<EdgeFate>,
    unpaired: Vec<UnpairedRequirement>,
    /// Every name an edge binds in either lock.
    recorded: HashSet<String>,
    /// Every `(name, version)` an edge binds before the resolve.
    bound_before: HashSet<(String, String)>,
    /// The records binding each `(name, version)` after the resolve, as reports name them.
    binders_after: BTreeMap<(String, String), BTreeSet<String>>,
}

/// One pairing stage's records, grouped by the stage's identity.
type Groups<K> = BTreeMap<K, (Vec<CopyBinding>, Vec<CopyBinding>)>;

impl CopyLedger {
    /// Reads the edges of the lock `before` the resolve (`None` without one) and `after` it.
    ///
    /// # Errors
    ///
    /// Returns the lock parse error of either side.
    pub(super) fn read<L: NodeLock>(before: Option<&str>, after: &str) -> Result<Self> {
        let before = before
            .map(L::copy_bindings)
            .transpose()?
            .unwrap_or_default();
        Ok(Self::from_bindings(before, L::copy_bindings(after)?))
    }

    /// Pairs every edge of `before` with its continuation in `after`, in three stages that each
    /// see only what the earlier ones left unpaired:
    ///
    /// 1. The same record and dependency key on both sides: an importer, or a snapshot whose full
    ///    key, peer suffix included, survived.
    /// 2. The same dependent package version and key, for a snapshot whose peer context changed.
    /// 3. The same dependent and key, for a dependent package that itself moved.
    ///
    /// Within stages 2 and 3 a group pairs only when its records bind exactly one version on each
    /// side; the old and new version are then one edge's move.
    /// After stage 3 a group bound only before is a dropped edge and one bound only after an added
    /// edge, and any other group is unpaired: no edge proves which version replaced which.
    pub(super) fn from_bindings(before: Vec<CopyBinding>, after: Vec<CopyBinding>) -> Self {
        let before: BTreeSet<CopyBinding> = before.into_iter().collect();
        let after: BTreeSet<CopyBinding> = after.into_iter().collect();
        let mut ledger = Self {
            recorded: before
                .iter()
                .chain(&after)
                .map(|binding| binding.name.clone())
                .collect(),
            bound_before: before
                .iter()
                .map(|binding| (binding.name.clone(), binding.version.clone()))
                .collect(),
            ..Self::default()
        };
        for binding in &after {
            ledger
                .binders_after
                .entry((binding.name.clone(), binding.version.clone()))
                .or_default()
                .insert(label(&binding.context));
        }

        // Same record
        let (before, after) = ledger.pair_stage(
            before.into_iter().collect(),
            after.into_iter().collect(),
            |binding| {
                (
                    binding.context.clone(),
                    binding.slot.clone(),
                    binding.name.clone(),
                )
            },
        );
        // Same dependent version, another peer context
        let (before, after) = ledger.pair_stage(before, after, |binding| {
            (
                Dependent::of(&binding.context),
                Dependent::version(&binding.context).map(str::to_string),
                binding.slot.clone(),
                binding.name.clone(),
            )
        });
        // Same dependent, which itself moved
        let groups = group(before, after, |binding| {
            (
                Dependent::of(&binding.context),
                binding.slot.clone(),
                binding.name.clone(),
            )
        });
        for ((dependent, _, name), (before, after)) in groups {
            if ledger.pair_group(&before, &after) {
                continue;
            }
            if after.is_empty() {
                ledger.fates.extend(before.iter().map(|binding| EdgeFate {
                    dependent: dependent.clone(),
                    holders: label(&binding.context),
                    name: binding.name.clone(),
                    from: binding.version.clone(),
                    fate: Fate::Dropped,
                }));
            } else if !before.is_empty() {
                ledger.fates.extend(before.iter().map(|binding| EdgeFate {
                    dependent: dependent.clone(),
                    holders: label(&binding.context),
                    name: binding.name.clone(),
                    from: binding.version.clone(),
                    fate: Fate::Unpaired,
                }));
                ledger.unpaired.push(UnpairedRequirement {
                    dependent: match &dependent {
                        Dependent::Importer(path) => format!("importer `{path}`"),
                        Dependent::Package(name) => name.clone(),
                    },
                    name,
                    before: versions(&before),
                    after: versions(&after),
                });
            }
        }
        ledger
    }

    /// Pairs every group `key` forms that binds one version on each side, returning the records of
    /// the other groups.
    fn pair_stage<K: Ord>(
        &mut self,
        before: Vec<CopyBinding>,
        after: Vec<CopyBinding>,
        key: impl Fn(&CopyBinding) -> K,
    ) -> (Vec<CopyBinding>, Vec<CopyBinding>) {
        let mut rest_before = Vec::new();
        let mut rest_after = Vec::new();
        for (before, after) in group(before, after, key).into_values() {
            if !self.pair_group(&before, &after) {
                rest_before.extend(before);
                rest_after.extend(after);
            }
        }
        (rest_before, rest_after)
    }

    /// Records the group's edges as one move when each side binds exactly one version.
    fn pair_group(&mut self, before: &[CopyBinding], after: &[CopyBinding]) -> bool {
        let (before_versions, after_versions) = (versions(before), versions(after));
        let (Some(to), 1, 1) = (
            after_versions.first(),
            before_versions.len(),
            after_versions.len(),
        ) else {
            return false;
        };
        let holders: BTreeSet<String> = after
            .iter()
            .map(|binding| label(&binding.context))
            .collect();
        let holders = holders.into_iter().collect::<Vec<_>>().join(", ");
        self.fates.extend(before.iter().map(|binding| EdgeFate {
            dependent: Dependent::of(&binding.context),
            holders: holders.clone(),
            name: binding.name.clone(),
            from: binding.version.clone(),
            fate: Fate::Bound(to.clone()),
        }));
        true
    }

    /// Whether either lock records an edge binding `name`.
    pub(super) fn records(&self, name: &str) -> bool {
        self.recorded.contains(name)
    }

    /// Every distinct net move of an edge, sorted.
    pub(super) fn moves(&self) -> Vec<CopyMove> {
        let moves: BTreeSet<(&str, &str, &str)> = self
            .fates
            .iter()
            .filter_map(|edge| match &edge.fate {
                Fate::Bound(to) if *to != edge.from => {
                    Some((edge.name.as_str(), edge.from.as_str(), to.as_str()))
                }
                Fate::Bound(_) | Fate::Dropped | Fate::Unpaired => None,
            })
            .collect();
        moves
            .into_iter()
            .map(|(name, from, to)| CopyMove {
                name: name.to_string(),
                from: from.to_string(),
                to: to.to_string(),
            })
            .collect()
    }

    /// Every requirement the resolve rewrote beyond a single pairing.
    pub(super) fn unpaired(&self) -> &[UnpairedRequirement] {
        &self.unpaired
    }

    /// What the resolve did to the copy `change` moves, judged by every edge that bound it.
    pub(super) fn landing(&self, change: &Change) -> CopyLanding {
        let name = change.package.name.as_str();
        let (from, to) = (change.from.as_str(), change.to.as_str());
        let mut landing = CopyLanding::default();
        for edge in self
            .fates
            .iter()
            .filter(|edge| edge.name == name && edge.from == from)
        {
            landing.recorded = true;
            match &edge.fate {
                Fate::Bound(version) if version == to => {
                    landing.reached.insert(edge.holders.clone());
                }
                Fate::Bound(version) => {
                    landing
                        .behind
                        .entry(edge.holders.clone())
                        .or_default()
                        .insert(version.clone());
                    if let Dependent::Package(name) = &edge.dependent {
                        landing.holders.insert(name.clone());
                    }
                }
                Fate::Dropped => {}
                Fate::Unpaired => {
                    landing.unpaired.insert(edge.holders.clone());
                }
            }
        }
        if !self
            .bound_before
            .contains(&(name.to_string(), to.to_string()))
        {
            landing.added_by = self
                .binders_after
                .get(&(name.to_string(), to.to_string()))
                .cloned()
                .unwrap_or_default();
        }
        landing
    }
}

/// What a resolve did to one planned copy, read from the edges that bound it
/// ([`CopyLedger::landing`]).
#[derive(Debug, Default)]
pub(super) struct CopyLanding {
    /// Whether any edge bound the copy before the resolve.
    recorded: bool,
    /// The records whose edge moved to exactly the planned target.
    reached: BTreeSet<String>,
    /// The records whose edge binds another version, with the versions it binds.
    behind: BTreeMap<String, BTreeSet<String>>,
    /// The names of the packages among `behind`.
    holders: BTreeSet<String>,
    /// The records whose edge the lock does not pair.
    unpaired: BTreeSet<String>,
    /// The records binding a copy at the target the lock did not hold before: a copy added
    /// elsewhere when no edge of the planned copy moved there.
    added_by: BTreeSet<String>,
}

impl CopyLanding {
    /// Whether the copy was replaced: some edge moved to exactly the target, and every other edge
    /// either did too or no longer requires the name.
    pub(super) fn replaced(&self) -> bool {
        !self.reached.is_empty() && self.behind.is_empty() && self.unpaired.is_empty()
    }

    /// Whether the edges explain a copy the resolve did not replace in more detail than an
    /// ordinary hold: it advanced for only some dependents, its dependents settled at several
    /// versions, the lock does not pair one of its edges, or a copy at the target was added for
    /// other dependents.
    pub(super) fn explains_hold(&self) -> bool {
        self.recorded
            && !self.replaced()
            && (!self.reached.is_empty()
                || (!self.behind.is_empty() && self.settled().is_none())
                || !self.unpaired.is_empty()
                || !self.added_by.is_empty())
    }

    /// The one version every edge left behind binds, `None` when no edge was left behind or they
    /// bind several.
    pub(super) fn settled(&self) -> Option<&str> {
        let versions: BTreeSet<&str> = self.behind.values().flatten().map(String::as_str).collect();
        match versions.into_iter().collect::<Vec<_>>().as_slice() {
            [version] => Some(version),
            _ => None,
        }
    }

    /// The first package left behind, the requirer whose range kept the copy.
    pub(super) fn holder(&self) -> Option<&str> {
        self.holders.first().map(String::as_str)
    }

    /// The hold detail for a copy the resolve did not replace ([`Self::explains_hold`]).
    pub(super) fn hold_detail(&self, change: &Change) -> String {
        let name = change.package.name.as_str();
        let (from, to) = (change.from.as_str(), change.to.as_str());
        let mut parts = Vec::new();
        if !self.reached.is_empty() {
            parts.push(format!(
                "the {name} {from} copy moved to {to} only for {}",
                listed(self.reached.iter().cloned())
            ));
        }
        for (holders, versions) in &self.behind {
            parts.push(format!(
                "{holders} still resolves {name} {}",
                versions.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.unpaired.is_empty() {
            parts.push(format!(
                "the lock does not pair the {name} {from} requirement of {} with a new version",
                listed(self.unpaired.iter().cloned())
            ));
        }
        if self.reached.is_empty() && !self.added_by.is_empty() {
            parts.push(format!(
                "{name} {to} is a new copy, bound by {}",
                listed(self.added_by.iter().cloned())
            ));
        }
        parts.join("; ")
    }
}

/// Groups `before` and `after` by `key`.
fn group<K: Ord>(
    before: Vec<CopyBinding>,
    after: Vec<CopyBinding>,
    key: impl Fn(&CopyBinding) -> K,
) -> Groups<K> {
    let mut groups: Groups<K> = BTreeMap::new();
    for binding in before {
        groups.entry(key(&binding)).or_default().0.push(binding);
    }
    for binding in after {
        groups.entry(key(&binding)).or_default().1.push(binding);
    }
    groups
}

fn versions(bindings: &[CopyBinding]) -> BTreeSet<String> {
    bindings
        .iter()
        .map(|binding| binding.version.clone())
        .collect()
}

/// Names up to [`NAMED_DEPENDENTS`] labels and counts the rest.
fn listed(labels: impl Iterator<Item = String>) -> String {
    let labels: Vec<String> = labels.collect();
    match labels.split_at_checked(NAMED_DEPENDENTS) {
        Some((named, rest)) if !rest.is_empty() => {
            format!("{} and {} more", named.join(", "), rest.len())
        }
        _ => labels.join(", "),
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::lock::Pnpm;
    use color_eyre::eyre;
    use cooldown_core::{PackageId, UpdateKind, Version};
    use indoc::indoc;

    fn ledger(before: &str, after: &str) -> eyre::Result<CopyLedger> {
        Ok(CopyLedger::read::<Pnpm>(Some(before), after)?)
    }

    /// A transitive change, which carries no declaring members.
    fn transitive(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(Pnpm::ID, name, None),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: false,
            members: Vec::new(),
        }
    }

    fn moves(ledger: &CopyLedger) -> Vec<(String, String, String)> {
        ledger
            .moves()
            .into_iter()
            .map(|CopyMove { name, from, to }| (name, from, to))
            .collect()
    }

    fn triple(name: &str, from: &str, to: &str) -> (String, String, String) {
        (name.to_string(), from.to_string(), to.to_string())
    }

    /// The issue's `seroval` shape: the root importer's 1.7.4 copy moves to 1.7.5, while the 1.6.11
    /// copy `solid-js` and `seroval-plugins` bind moves to 1.6.12, as `solid-js` itself moves to
    /// 1.9.18 and `seroval-plugins` keeps its version under a new peer context.
    pub(in crate::tool) const SEROVAL_BEFORE: &str = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              seroval:
                specifier: ^1.5.6
                version: 1.7.4
              seroval-plugins:
                specifier: ^1.5.6
                version: 1.7.4(seroval@1.7.4)
              solid-js:
                specifier: ^1.9.14
                version: 1.9.17

        packages:

          seroval-plugins@1.6.12:
            resolution: {integrity: sha512-a}

          seroval-plugins@1.7.4:
            resolution: {integrity: sha512-b}

          seroval@1.6.11:
            resolution: {integrity: sha512-c}

          seroval@1.7.4:
            resolution: {integrity: sha512-d}

          solid-js@1.9.17:
            resolution: {integrity: sha512-e}

        snapshots:

          seroval-plugins@1.6.12(seroval@1.6.11):
            dependencies:
              seroval: 1.6.11

          seroval-plugins@1.7.4(seroval@1.7.4):
            dependencies:
              seroval: 1.7.4

          seroval@1.6.11: {}

          seroval@1.7.4: {}

          solid-js@1.9.17:
            dependencies:
              seroval: 1.6.11
              seroval-plugins: 1.6.12(seroval@1.6.11)
    "};

    pub(in crate::tool) const SEROVAL_AFTER: &str = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              seroval:
                specifier: ^1.5.6
                version: 1.7.5
              seroval-plugins:
                specifier: ^1.5.6
                version: 1.7.6(seroval@1.7.5)
              solid-js:
                specifier: ^1.9.14
                version: 1.9.18

        packages:

          seroval-plugins@1.6.12:
            resolution: {integrity: sha512-a}

          seroval-plugins@1.7.6:
            resolution: {integrity: sha512-f}

          seroval@1.6.12:
            resolution: {integrity: sha512-g}

          seroval@1.7.5:
            resolution: {integrity: sha512-h}

          solid-js@1.9.18:
            resolution: {integrity: sha512-i}

        snapshots:

          seroval-plugins@1.6.12(seroval@1.6.12):
            dependencies:
              seroval: 1.6.12

          seroval-plugins@1.7.6(seroval@1.7.5):
            dependencies:
              seroval: 1.7.5

          seroval@1.6.12: {}

          seroval@1.7.5: {}

          solid-js@1.9.18:
            dependencies:
              seroval: 1.6.12
              seroval-plugins: 1.6.12(seroval@1.6.12)
    "};

    /// Each copy's move is read from its own edges: the 1.6.11 copy moved to 1.6.12, not to the
    /// 1.7.5 the root importer's copy reached, so a planned 1.6.11 → 1.7.5 is no replacement.
    #[test]
    fn the_seroval_copies_move_by_their_own_edges() -> eyre::Result<()> {
        let ledger = ledger(SEROVAL_BEFORE, SEROVAL_AFTER)?;

        // Every edge move, the old copy's included; `seroval-plugins` 1.6.12 only changed its peer
        // context, which is no move.
        assert_eq!(
            moves(&ledger),
            vec![
                triple("seroval", "1.6.11", "1.6.12"),
                triple("seroval", "1.7.4", "1.7.5"),
                triple("seroval-plugins", "1.7.4", "1.7.6"),
                triple("solid-js", "1.9.17", "1.9.18"),
            ]
        );
        assert!(ledger.unpaired().is_empty(), "{:?}", ledger.unpaired());

        // The old copy's dependents still resolve 1.6.12, and the 1.7.5 copy they never reached is
        // bound by others.
        let landing = ledger.landing(&transitive("seroval", "1.6.11", "1.7.5"));
        assert!(!landing.replaced(), "{landing:?}");
        assert!(landing.explains_hold(), "{landing:?}");
        assert_eq!(landing.holder(), Some("seroval-plugins"));
        let detail = landing.hold_detail(&transitive("seroval", "1.6.11", "1.7.5"));
        assert!(
            detail.contains("solid-js@1.9.18 still resolves seroval 1.6.12"),
            "{detail}"
        );
        assert!(
            detail.contains("seroval 1.7.5 is a new copy, bound by importer `.`"),
            "{detail}"
        );

        // The move the edges do prove lands.
        assert!(
            ledger
                .landing(&transitive("seroval", "1.6.11", "1.6.12"))
                .replaced()
        );
        Ok(())
    }

    /// The issue's `inline-style-parser` shape: `style-to-object` 1.0.14 keeps 0.2.7, and only a new
    /// `style-to-object` 2.0.2 binds the new 0.2.9 copy, which is a copy added elsewhere.
    #[test]
    fn a_target_copy_added_for_another_dependent_is_no_move() -> eyre::Result<()> {
        let before = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  style-to-js:
                    specifier: ^1.1.21
                    version: 1.1.21

            packages:

              inline-style-parser@0.2.7:
                resolution: {integrity: sha512-a}

              style-to-js@1.1.21:
                resolution: {integrity: sha512-b}

              style-to-object@1.0.14:
                resolution: {integrity: sha512-c}

            snapshots:

              inline-style-parser@0.2.7: {}

              style-to-js@1.1.21:
                dependencies:
                  style-to-object: 1.0.14

              style-to-object@1.0.14:
                dependencies:
                  inline-style-parser: 0.2.7
        "};
        let after = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  style-to-js:
                    specifier: ^1.1.21
                    version: 1.1.21
                  style-to-object:
                    specifier: ^2.0.2
                    version: 2.0.2

            packages:

              inline-style-parser@0.2.7:
                resolution: {integrity: sha512-a}

              inline-style-parser@0.2.9:
                resolution: {integrity: sha512-d}

              style-to-js@1.1.21:
                resolution: {integrity: sha512-b}

              style-to-object@1.0.14:
                resolution: {integrity: sha512-c}

              style-to-object@2.0.2:
                resolution: {integrity: sha512-e}

            snapshots:

              inline-style-parser@0.2.7: {}

              inline-style-parser@0.2.9: {}

              style-to-js@1.1.21:
                dependencies:
                  style-to-object: 1.0.14

              style-to-object@1.0.14:
                dependencies:
                  inline-style-parser: 0.2.7

              style-to-object@2.0.2:
                dependencies:
                  inline-style-parser: 0.2.9
        "};
        let ledger = ledger(before, after)?;

        // A new copy moves nothing.
        assert_eq!(moves(&ledger), Vec::<(String, String, String)>::new());

        let planned = transitive("inline-style-parser", "0.2.7", "0.2.9");
        let landing = ledger.landing(&planned);
        assert!(!landing.replaced(), "{landing:?}");
        assert!(landing.explains_hold(), "{landing:?}");
        assert_eq!(landing.holder(), Some("style-to-object"));
        assert_eq!(
            landing.hold_detail(&planned),
            "style-to-object@1.0.14 still resolves inline-style-parser 0.2.7; \
             inline-style-parser 0.2.9 is a new copy, bound by style-to-object@2.0.2"
        );
        Ok(())
    }

    /// Two dependents of one copy, `p` and `q`, around a planned move of `leaf`.
    pub(in crate::tool) fn shared_copy(p_binds: &str, q_binds: &str) -> String {
        let leaves: BTreeSet<&str> = BTreeSet::from([p_binds, q_binds]);
        let leaves = leaves
            .into_iter()
            .map(|version| {
                format!("\n  leaf@{version}:\n    resolution: {{integrity: sha512-{version}}}\n")
            })
            .collect::<Vec<_>>()
            .concat();
        format!(
            indoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      p:
                        specifier: ^1.0.0
                        version: 1.0.0
                      q:
                        specifier: ^1.0.0
                        version: 1.0.0

                packages:

                  p@1.0.0:
                    resolution: {{integrity: sha512-p}}

                  q@1.0.0:
                    resolution: {{integrity: sha512-q}}
                {}
                snapshots:

                  p@1.0.0:
                    dependencies:
                      leaf: {}

                  q@1.0.0:
                    dependencies:
                      leaf: {}
            "},
            leaves, p_binds, q_binds
        )
    }

    /// A copy replaced for every dependent lands; one advanced for only some dependents does not,
    /// and each dependent's own move is a move of its own.
    #[test]
    fn a_copy_advanced_for_only_some_dependents_is_not_replaced() -> eyre::Result<()> {
        let planned = transitive("leaf", "1.0.0", "1.2.0");

        // Replaced for both
        let replaced = ledger(
            &shared_copy("1.0.0", "1.0.0"),
            &shared_copy("1.2.0", "1.2.0"),
        )?;
        assert!(replaced.landing(&planned).replaced());

        // Partly advanced
        let partly = ledger(
            &shared_copy("1.0.0", "1.0.0"),
            &shared_copy("1.2.0", "1.0.1"),
        )?;
        let landing = partly.landing(&planned);
        assert!(!landing.replaced(), "{landing:?}");
        assert!(landing.explains_hold(), "{landing:?}");
        assert_eq!(
            landing.hold_detail(&planned),
            "the leaf 1.0.0 copy moved to 1.2.0 only for p@1.0.0; q@1.0.0 still resolves leaf 1.0.1"
        );
        assert_eq!(
            moves(&partly),
            vec![
                triple("leaf", "1.0.0", "1.0.1"),
                triple("leaf", "1.0.0", "1.2.0")
            ]
        );

        // Overshoot: the planned target is exact
        let overshot = ledger(
            &shared_copy("1.0.0", "1.0.0"),
            &shared_copy("1.3.0", "1.3.0"),
        )?;
        assert!(!overshot.landing(&planned).replaced());
        Ok(())
    }

    /// Two persisting copies of one dependent that each move their own binding are two moves, not
    /// one chained across them: the aggregate `{1.0, 1.1} → {1.1, 1.2}` would read as 1.0 → 1.2.
    #[test]
    fn persisting_dependent_copies_move_their_own_bindings() -> eyre::Result<()> {
        let lock = |first: &str, second: &str| {
            format!(
                indoc! {"
                    lockfileVersion: '9.0'

                    importers:

                      .:
                        dependencies:
                          p:
                            specifier: ^1.0.0
                            version: 1.0.0
                          q:
                            specifier: ^2.0.0
                            version: 2.0.0

                    snapshots:

                      p@1.0.0:
                        dependencies:
                          leaf: {}

                      p@2.0.0:
                        dependencies:
                          leaf: {}

                      q@2.0.0:
                        dependencies:
                          p: 2.0.0
                "},
                first, second
            )
        };
        let ledger = ledger(&lock("1.0.0", "1.1.0"), &lock("1.1.0", "1.2.0"))?;

        assert_eq!(
            moves(&ledger),
            vec![
                triple("leaf", "1.0.0", "1.1.0"),
                triple("leaf", "1.1.0", "1.2.0")
            ]
        );
        assert!(
            !ledger
                .landing(&transitive("leaf", "1.0.0", "1.2.0"))
                .replaced()
        );
        Ok(())
    }

    /// Two versions of one dependent whose peer contexts were both rewritten pair within each
    /// version, so the moves stay apart instead of merging into one unpaired requirement.
    #[test]
    fn rewritten_peer_contexts_pair_within_their_package_version() -> eyre::Result<()> {
        let lock = |old: &str, new: &str| {
            format!(
                indoc! {"
                    lockfileVersion: '9.0'

                    importers:

                      .:
                        dependencies:
                          host:
                            specifier: ^1.0.0
                            version: 1.0.0

                    snapshots:

                      plugin@7.1.0(core@{new}):
                        dependencies:
                          core: {new}

                      plugin@7.2.0(core@{old}):
                        dependencies:
                          core: {old}
                "},
                old = old,
                new = new
            )
        };
        let ledger = ledger(&lock("7.29.7", "8.0.9"), &lock("7.29.8", "8.0.10"))?;

        assert_eq!(
            moves(&ledger),
            vec![
                triple("core", "7.29.7", "7.29.8"),
                triple("core", "8.0.9", "8.0.10"),
            ]
        );
        assert!(ledger.unpaired().is_empty(), "{:?}", ledger.unpaired());
        Ok(())
    }

    /// `a` stops requiring `leaf`, and the two peer contexts of `p`, binding `leaf` 1.0.0 and
    /// 1.1.0, are both rewritten to bind 1.0.1 and 1.1.1.
    pub(in crate::tool) const UNPAIRED_BEFORE: &str = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              a:
                specifier: ^1.0.0
                version: 1.0.0

        snapshots:

          a@1.0.0:
            dependencies:
              leaf: 1.3.0

          p@1.0.0(peer@1.0.0):
            dependencies:
              leaf: 1.0.0

          p@1.0.0(peer@2.0.0):
            dependencies:
              leaf: 1.1.0
    "};

    pub(in crate::tool) const UNPAIRED_AFTER: &str = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              a:
                specifier: ^1.0.0
                version: 1.1.0

        snapshots:

          a@1.1.0: {}

          p@1.0.0(peer@1.0.1):
            dependencies:
              leaf: 1.0.1

          p@1.0.0(peer@2.0.1):
            dependencies:
              leaf: 1.1.1
    "};

    /// A dependent whose rewritten records bind several versions on each side pairs no old version
    /// with a new one: the change is reported unpaired, never as a move, and credits nothing.
    /// An edge whose dependent stopped requiring the name is dropped, not moved.
    #[test]
    fn rewritten_records_with_several_versions_pair_nothing() -> eyre::Result<()> {
        let ledger = ledger(UNPAIRED_BEFORE, UNPAIRED_AFTER)?;

        // Only the importer's own move is one; `a`'s dropped edge and `p`'s unpaired ones are not.
        assert_eq!(moves(&ledger), vec![triple("a", "1.0.0", "1.1.0")]);
        assert_eq!(
            ledger.unpaired(),
            [UnpairedRequirement {
                dependent: "p".to_string(),
                name: "leaf".to_string(),
                before: BTreeSet::from(["1.0.0".to_string(), "1.1.0".to_string()]),
                after: BTreeSet::from(["1.0.1".to_string(), "1.1.1".to_string()]),
            }]
        );
        let unpaired = ledger.landing(&transitive("leaf", "1.0.0", "1.0.1"));
        assert!(!unpaired.replaced(), "{unpaired:?}");
        assert!(unpaired.explains_hold(), "{unpaired:?}");
        let dropped = ledger.landing(&transitive("leaf", "1.3.0", "1.4.0"));
        assert!(!dropped.replaced(), "{dropped:?}");
        assert!(!dropped.explains_hold(), "{dropped:?}");
        Ok(())
    }

    /// An npm alias keeps its own slot, so one dependent binding a package twice, once through an
    /// alias, moves each binding on its own.
    #[test]
    fn an_alias_slot_is_its_own_edge() -> eyre::Result<()> {
        let lock = |old: &str| {
            format!(
                indoc! {"
                    lockfileVersion: '9.0'

                    importers:

                      .:
                        dependencies:
                          p:
                            specifier: ^1.0.0
                            version: 1.0.0

                    snapshots:

                      p@1.0.0:
                        dependencies:
                          leaf: 2.0.0
                          old-leaf: leaf@{}
                "},
                old
            )
        };
        let ledger = ledger(&lock("1.0.0"), &lock("1.1.0"))?;

        assert_eq!(moves(&ledger), vec![triple("leaf", "1.0.0", "1.1.0")]);
        assert!(
            ledger
                .landing(&transitive("leaf", "1.0.0", "1.1.0"))
                .replaced()
        );
        Ok(())
    }

    /// A lock that records no edge for a name gives its copies no evidence.
    #[test]
    fn a_name_without_edges_is_unrecorded() -> eyre::Result<()> {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  p:
                    specifier: ^1.0.0
                    version: 1.0.0

            packages:

              leaf@1.0.0:
                resolution: {integrity: sha512-leaf}
        "};
        let ledger = ledger(lock, lock)?;

        assert!(ledger.records("p"));
        assert!(!ledger.records("leaf"));
        let landing = ledger.landing(&transitive("leaf", "1.0.0", "1.1.0"));
        assert!(
            !landing.replaced() && !landing.explains_hold(),
            "{landing:?}"
        );
        Ok(())
    }
}
