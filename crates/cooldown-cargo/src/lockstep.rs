//! Lockstep families: crates.io nodes bound to each other by exact `=x.y.z` requirements.
//!
//! A family such as wasm-bindgen's (`js-sys` and `web-sys` pin `wasm-bindgen = "=0.2.129"`) or
//! a snapshot-versioned framework moves only as a whole: while one locked member still pins an
//! old version of another, Cargo refuses any step that moves part of the family.
//! A precise pin or a seeded resolve releases only the nodes it names, so a refusal that names a
//! family member is answered by releasing that member's lockstep neighbourhood with it.
//!
//! The edges come from every graph the run reads, with no Cargo call of their own: the
//! dependency read before planning names the edges of every node an apply starts with, and each
//! later resolve those of the nodes it lands.
//! A registry package's requirements never change, so an edge read once stays true for the run.
//! An edge active in one project's graph may be inactive in another's (an optional dependency
//! one enables), which can only release a node the refusal did not strictly need; the
//! acceptance rule still judges every released node.

use crate::cargocmd::ResolvedGraph;
use crate::lockfile::{CargoLock, PackageKey};
use cooldown_core::GraphHoldKind;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, PoisonError};

/// What an attempt does with one node of a lockstep family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The attempt pins or seeds the node to a target of its own.
    Subject,
    /// The attempt leaves the node where it is: a declared crate, a planned target, or the
    /// original node of a change the attempt gave up on.
    Held,
    /// The attempt may release the node.
    Free,
}

/// How many times one attempt is retried with more of a lockstep family released, as many as a
/// refused pin's blocker rounds.
const FAMILY_RETRIES: usize = 3;

/// The lockstep family members an attempt releases, grown refusal by refusal.
#[derive(Debug, Default)]
pub(crate) struct Released {
    nodes: Vec<PackageKey>,
    retries: usize,
}

impl Released {
    pub(crate) fn nodes(&self) -> &[PackageKey] {
        &self.nodes
    }

    /// Adds `family` for one more retry and returns whether the attempt is retried: not once its
    /// retries are spent, nor when `family` adds no node, since the same release would only meet
    /// the same refusal.
    pub(crate) fn grow(&mut self, family: Vec<PackageKey>) -> bool {
        if self.retries == FAMILY_RETRIES {
            return false;
        }
        let before = self.nodes.len();
        for node in family {
            if !self.nodes.contains(&node) {
                self.nodes.push(node);
            }
        }
        let grew = self.nodes.len() > before;
        self.retries += usize::from(grew);
        grew
    }
}

/// The run's active exact-pin edges between non-member registry nodes, in both directions.
#[derive(Debug, Default)]
pub(crate) struct LockstepEdges {
    edges: Mutex<Edges>,
}

#[derive(Debug, Default)]
struct Edges {
    /// Each pinned node's requirers whose `=` requirement pins it at exactly its version.
    parents: HashMap<PackageKey, BTreeSet<PackageKey>>,
    /// Each requirer's exact-pinned dependencies.
    children: HashMap<PackageKey, BTreeSet<PackageKey>>,
    /// How many distinct edges are recorded.
    len: usize,
}

impl LockstepEdges {
    /// Adds the exact-pin edges among `graph`'s attributed
    /// [`hold_edges`](ResolvedGraph::hold_edges).
    ///
    /// A workspace member's own exact pin is not an edge here: the graph attributes holds to
    /// non-member requirers only, and a declared crate never moves outside the plan anyway.
    pub(crate) fn record(&self, graph: &ResolvedGraph) {
        let mut edges = self.edges.lock().unwrap_or_else(PoisonError::into_inner);
        for ((name, version), holds) in &graph.hold_edges {
            let pinned = PackageKey::new(name.clone(), version.clone());
            for hold in holds
                .iter()
                .filter(|hold| hold.kind == GraphHoldKind::Ceiling)
            {
                let requirer =
                    PackageKey::new(hold.requirer.clone(), hold.requirer_version.as_str());
                let new = edges
                    .parents
                    .entry(pinned.clone())
                    .or_default()
                    .insert(requirer.clone());
                edges.len += usize::from(new);
                edges
                    .children
                    .entry(requirer)
                    .or_default()
                    .insert(pinned.clone());
            }
        }
    }

    /// How many distinct edges the run has recorded, which only grows.
    pub(crate) fn len(&self) -> usize {
        self.edges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len
    }

    /// The nodes to release beside an attempt Cargo refused over `refused`, the locked nodes its
    /// diagnostic names.
    ///
    /// Each refused node stays locked while any requirer still pins it exactly, so the release
    /// climbs every chain of exact requirers above it (an undeclared `R` pinning `Q` pinning `P`
    /// keeps `P` as firmly as `Q` does).
    /// A refused node the attempt holds itself releases nothing: the refusal is over that node,
    /// which the attempt must leave where it is, so no release can land it.
    /// Every node an exact requirer in the climbed set pins below it must be free to follow the
    /// requirer's successor, so the release then descends through their exact dependencies too,
    /// stopping at held nodes.
    /// Co-parents of those dependencies are not climbed: a dependency that need not move keeps
    /// its other requirers where they are.
    ///
    /// Only free family members are returned, sorted, and only those `lock` carries on
    /// crates.io: a refused node bound by no exact edge is left to the caller's own blocker
    /// rules.
    pub(crate) fn releases(
        &self,
        lock: &CargoLock,
        refused: &[PackageKey],
        role: impl Fn(&PackageKey) -> Role,
    ) -> Vec<PackageKey> {
        let edges = self.edges.lock().unwrap_or_else(PoisonError::into_inner);
        let held = |node: &PackageKey| role(node) == Role::Held;
        let mut climbed = BTreeSet::new();
        for node in refused.iter().filter(|node| !held(node)) {
            climbed.extend(closure([node.clone()], &edges.parents, |_| false));
        }
        closure(climbed, &edges.children, held)
            .into_iter()
            .filter(|node| {
                (edges.parents.contains_key(node) || edges.children.contains_key(node))
                    && lock.has_crates_io_package(&node.name, &node.version)
                    && role(node) == Role::Free
            })
            .collect()
    }
}

/// `start` and every node reachable from it along `edges`, without following edges out of a
/// node `stop` names.
fn closure(
    start: impl IntoIterator<Item = PackageKey>,
    edges: &HashMap<PackageKey, BTreeSet<PackageKey>>,
    stop: impl Fn(&PackageKey) -> bool,
) -> BTreeSet<PackageKey> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<PackageKey> = start.into_iter().collect();
    while let Some(node) = queue.pop() {
        if !stop(&node)
            && let Some(next) = edges.get(&node)
        {
            queue.extend(next.iter().filter(|next| !seen.contains(*next)).cloned());
        }
        seen.insert(node);
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use cooldown_core::{GraphHoldEdge, Version};
    use std::collections::HashSet;
    use std::fmt::Write as _;

    fn node(name: &str, version: &str) -> PackageKey {
        PackageKey::new(name, version)
    }

    /// A `(name, version)` node.
    type Node<'a> = (&'a str, &'a str);

    /// Edges from `(requirer, pinned)` node pairs, each an active exact pin.
    fn edges(pins: &[(Node<'_>, Node<'_>)]) -> LockstepEdges {
        let mut hold_edges: HashMap<(String, String), Vec<GraphHoldEdge>> = HashMap::new();
        for ((requirer, requirer_version), (name, version)) in pins {
            hold_edges
                .entry(((*name).to_owned(), (*version).to_owned()))
                .or_default()
                .push(GraphHoldEdge {
                    requirer: (*requirer).to_owned(),
                    requirer_version: Version::new(*requirer_version),
                    requirement: format!("={version}"),
                    bound: Version::new(*version),
                    kind: GraphHoldKind::Ceiling,
                });
        }
        let graph = ResolvedGraph {
            packages: HashMap::new(),
            roots: HashSet::new(),
            generated_roots: HashSet::new(),
            edges: HashMap::new(),
            exact_pins: HashSet::new(),
            graph_ceilings: HashSet::new(),
            ceiling_requirers: HashMap::new(),
            hold_edges,
            graph_floors: HashMap::new(),
            declared_bounds: HashMap::new(),
            declared_requirements: HashMap::new(),
            rust_versions: HashMap::new(),
            workspace_rust_version: None,
        };
        let edges = LockstepEdges::default();
        edges.record(&graph);
        edges
    }

    fn lock(nodes: &[(&str, &str)]) -> eyre::Result<CargoLock> {
        let mut text = String::from("version = 4\n");
        for (name, version) in nodes {
            write!(
                text,
                "{}",
                indoc::formatdoc! {r#"

                    [[package]]
                    name = "{name}"
                    version = "{version}"
                    source = "{source}"
                "#, source = crate::lockfile::CRATES_IO_SOURCE}
            )?;
        }
        Ok(CargoLock::parse(&text)?)
    }

    /// The wasm-bindgen shape: a refusal naming `wasm-bindgen` releases every locked crate that
    /// pins it exactly and the macro crates pinned below them, but not the subject being pinned.
    #[test]
    fn a_refused_member_releases_its_exact_requirers_and_their_pinned_dependencies()
    -> eyre::Result<()> {
        let edges = edges(&[
            (("js-sys", "0.3.106"), ("wasm-bindgen", "0.2.129")),
            (("web-sys", "0.3.106"), ("wasm-bindgen", "0.2.129")),
            (("web-sys", "0.3.106"), ("js-sys", "0.3.106")),
            (("futures", "0.4.79"), ("js-sys", "0.3.106")),
            (("futures", "0.4.79"), ("wasm-bindgen", "0.2.129")),
            (("wasm-bindgen", "0.2.129"), ("macro", "0.2.129")),
        ]);
        let lock = lock(&[
            ("futures", "0.4.79"),
            ("js-sys", "0.3.106"),
            ("macro", "0.2.129"),
            ("wasm-bindgen", "0.2.129"),
            ("web-sys", "0.3.106"),
        ])?;
        let subject = node("futures", "0.4.79");
        let released = edges.releases(&lock, &[node("wasm-bindgen", "0.2.129")], |candidate| {
            if *candidate == subject {
                Role::Subject
            } else {
                Role::Free
            }
        });
        assert_eq!(
            released,
            [
                node("js-sys", "0.3.106"),
                node("macro", "0.2.129"),
                node("wasm-bindgen", "0.2.129"),
                node("web-sys", "0.3.106"),
            ]
        );
        Ok(())
    }

    /// A refused node no exact edge binds is no family member, so nothing is released for it.
    #[test]
    fn a_refused_node_outside_any_family_releases_nothing() -> eyre::Result<()> {
        let edges = edges(&[(("q", "1.0.0"), ("p", "1.0.0"))]);
        let lock = lock(&[("blocker", "1.0.0"), ("p", "1.0.0"), ("q", "1.0.0")])?;
        let released = edges.releases(&lock, &[node("blocker", "1.0.0")], |_| Role::Free);
        assert_eq!(released, []);
        Ok(())
    }

    /// A chain of exact requirers is climbed to its top: `R` pins `Q` pins `P`, so a refusal
    /// naming `P` releases `R` too, or `R` would keep the old `Q` and with it the old `P`.
    #[test]
    fn a_chain_of_exact_requirers_is_released_to_its_top() -> eyre::Result<()> {
        let edges = edges(&[
            (("r", "1.0.0"), ("q", "1.0.0")),
            (("q", "1.0.0"), ("p", "1.0.0")),
        ]);
        let lock = lock(&[("p", "1.0.0"), ("q", "1.0.0"), ("r", "1.0.0")])?;
        let released = edges.releases(&lock, &[node("p", "1.0.0")], |_| Role::Free);
        assert_eq!(
            released,
            [node("p", "1.0.0"), node("q", "1.0.0"), node("r", "1.0.0")]
        );
        Ok(())
    }

    /// A refused node the attempt holds releases nothing, but a held node above a refused one
    /// only stays locked itself: the edge it pins through may be one the new graph no longer
    /// activates, so the rest of the chain is still released.
    #[test]
    fn only_a_held_refused_node_releases_nothing() -> eyre::Result<()> {
        let edges = edges(&[
            (("declared", "1.0.0"), ("q", "1.0.0")),
            (("q", "1.0.0"), ("p", "1.0.0")),
            (("declared", "1.0.0"), ("helper", "1.0.0")),
        ]);
        let lock = lock(&[
            ("declared", "1.0.0"),
            ("helper", "1.0.0"),
            ("p", "1.0.0"),
            ("q", "1.0.0"),
        ])?;
        let declared = node("declared", "1.0.0");
        let role = |candidate: &PackageKey| {
            if *candidate == declared {
                Role::Held
            } else {
                Role::Free
            }
        };
        assert_eq!(
            edges.releases(&lock, std::slice::from_ref(&declared), role),
            []
        );
        // The descent stops at the held node, so its other dependency stays locked.
        assert_eq!(
            edges.releases(&lock, &[node("p", "1.0.0")], role),
            [node("p", "1.0.0"), node("q", "1.0.0")]
        );
        Ok(())
    }

    /// Only the requirers above the refused node are climbed: a helper pinned below a released
    /// requirer is released with it, but the helper's other exact requirer stays locked, and so
    /// does every node `keep` holds or the lock does not carry.
    #[test]
    fn co_requirers_of_a_pinned_dependency_and_kept_nodes_stay_locked() -> eyre::Result<()> {
        let edges = edges(&[
            (("q", "1.0.0"), ("p", "1.0.0")),
            (("q", "1.0.0"), ("helper", "1.0.0")),
            (("other", "1.0.0"), ("helper", "1.0.0")),
            (("q", "1.0.0"), ("reached", "2.0.0")),
            (("q", "1.0.0"), ("gone", "1.0.0")),
        ]);
        let lock = lock(&[
            ("helper", "1.0.0"),
            ("other", "1.0.0"),
            ("p", "1.0.0"),
            ("q", "1.0.0"),
            ("reached", "2.0.0"),
        ])?;
        let reached = node("reached", "2.0.0");
        let released = edges.releases(&lock, &[node("p", "1.0.0")], |candidate| {
            if *candidate == reached {
                Role::Held
            } else {
                Role::Free
            }
        });
        assert_eq!(
            released,
            [
                node("helper", "1.0.0"),
                node("p", "1.0.0"),
                node("q", "1.0.0")
            ]
        );
        Ok(())
    }
}
