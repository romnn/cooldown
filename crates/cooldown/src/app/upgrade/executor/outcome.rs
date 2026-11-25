//! What an `upgrade` pass settled for each dependency copy its first plan read, which `outdated`
//! reports.
//!
//! The pass's rows are events: a planned move, a hold, a lock entry another move carried.
//! A report row is a claim about state, what the copy's declaring members bind once the pass is
//! over, so the copies are settled here, where the graph the pass left can confirm a landing.
//! Only a move the pass made of the copy itself counts: a version the copy reaches as a side
//! effect of another move is not claimed, since no row proves the copy's own bindings went there.

use super::line_reached;
use super::planning::Judged;
use crate::app::change_key::ChangeKey;
use crate::app::{FetchedRelease, SecurityInfo, SkippedInfo, UpgradeItem};
use cooldown_core::{Change, Dependency, Diagnostic, PackageId, Version};
use std::collections::{HashMap, HashSet};

/// What an `upgrade` pass did with one copy of a dependency its first plan read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CopyOutcome {
    /// Every member declaring the copy binds `to` once the pass is over.
    Landed {
        to: String,
        security: Option<SecurityInfo>,
    },
    /// A plan tried moving the copy to `to`, and the pass held that move.
    Refused {
        to: String,
        skip: SkippedInfo,
        security: Option<SecurityInfo>,
    },
    /// The pass failed for the copy, or stopped before deciding a move it meant to make.
    Failed(Diagnostic),
    /// The pass decided nothing about the copy.
    Undecided,
}

/// The first-plan copy each change a plan tried moves, so the pass's rows trace back to it.
#[derive(Default)]
pub(super) struct Provenance {
    /// The copy's index in the first plan, under every key a row of the change can carry.
    origin: HashMap<ChangeKey, usize>,
    /// The deferred copies whose follower replan has not read the graph yet.
    awaiting: Vec<usize>,
    /// The failure each copy met where the pass read it, such as its release lookup.
    failures: HashMap<usize, Diagnostic>,
}

impl Provenance {
    /// Records that a plan tried `change`, which moves the first-plan copy `origin` of `own`.
    pub(super) fn tried(&mut self, change: &Change, origin: usize, own: &PackageId) {
        let key = ChangeKey::of(change);
        // A held Go move onto another major reports the module path the graph still requires.
        if change.package != *own {
            self.origin.insert(key.clone().reported_as(own), origin);
        }
        self.origin.insert(key, origin);
    }

    /// Records that the pass failed for the first-plan copy `origin` where it read it.
    pub(super) fn failed(&mut self, origin: usize, error: Diagnostic) {
        self.failures.entry(origin).or_insert(error);
    }

    /// Records the copies a follower replan decides once their requirers have settled.
    pub(super) fn await_replan(&mut self, copies: Vec<usize>) {
        self.awaiting = copies;
    }

    /// Records that the follower replan read the graph, which decides every deferred copy.
    pub(super) fn replanned(&mut self) {
        self.awaiting.clear();
    }
}

/// One dependency a first plan read, with its releases and verdict, and what the pass settled
/// for it.
pub(crate) struct PlannedCopy {
    pub(crate) fact: FetchedRelease<Judged>,
    /// `None` until the pass settles the copy, and again once its trial's state is discarded.
    pub(crate) outcome: Option<CopyOutcome>,
}

/// Settles what the pass did with each copy its first plan read, from its rows (`items`), its
/// errors, and the graph it left (`None` when that read failed).
///
/// For each copy the first outcome that holds wins:
///
/// - **Landed** by a move the pass tried for the copy and applied, which that graph still shows
///   (see [`copy_landed`]): a later pass can undo it.
/// - **Refused** by the hold of the lowest target a plan tried, whatever the reason.
/// - **Failed** by the error of such a move, the failure the pass met reading the copy, or the
///   failure that stopped the pass before it decided a copy it tried or deferred.
/// - **Undecided** otherwise, such as for a copy only another package's move carried.
pub(super) fn settle(
    copies: &mut [PlannedCopy],
    provenance: &Provenance,
    items: &[UpgradeItem],
    errors: &[Diagnostic],
    graph: Option<&[Dependency]>,
) {
    let mut traced: HashMap<usize, Vec<&UpgradeItem>> = HashMap::new();
    for item in items.iter().filter(|item| item.edge.is_none()) {
        if let Some(&copy) = provenance.origin.get(&ChangeKey::of_item(item)) {
            traced.entry(copy).or_default().push(item);
        }
    }
    let tried: HashSet<usize> = provenance.origin.values().copied().collect();
    let failure = errors.iter().rev().find(|error| error.package.is_none());
    for (index, copy) in copies.iter_mut().enumerate() {
        let dep = &copy.fact.dependency;
        let rows = traced.get(&index).map(Vec::as_slice).unwrap_or_default();
        let landing = rows.iter().find(|row| {
            // A Go move onto another major lands under that major's module path.
            let destination =
                PackageId::new(dep.package.tool, row.name.clone(), row.registry.clone());
            row.applied
                && !row.downgrade
                && graph.is_some_and(|graph| {
                    copy_landed(graph, dep, &destination, &Version::new(row.to.clone()))
                })
        });
        let outcome = if let Some(row) = landing {
            CopyOutcome::Landed {
                to: row.to.clone(),
                security: row.security.clone(),
            }
        } else if let Some((row, skip)) = nearest_hold(&copy.fact, rows) {
            CopyOutcome::Refused {
                to: row.to.clone(),
                skip: skip.clone(),
                security: row.security.clone(),
            }
        } else if let Some(error) = rows
            .iter()
            .find_map(|row| row.error.as_ref())
            .or_else(|| provenance.failures.get(&index))
        {
            CopyOutcome::Failed(error.clone())
        } else {
            match failure {
                Some(failure) if tried.contains(&index) || provenance.awaiting.contains(&index) => {
                    CopyOutcome::Failed(failure.clone())
                }
                _ => CopyOutcome::Undecided,
            }
        };
        copy.outcome = Some(outcome);
    }
}

/// The hold on the lowest target a plan tried for the copy `fact` describes, among its traced
/// `rows`: the nearest move the pass held, such as an in-line fallback below a held major.
fn nearest_hold<'a>(
    fact: &FetchedRelease<Judged>,
    rows: &[&'a UpgradeItem],
) -> Option<(&'a UpgradeItem, &'a SkippedInfo)> {
    let releases = fact
        .result
        .as_ref()
        .map(|judged| judged.releases.as_slice())
        .unwrap_or_default();
    rows.iter()
        .filter_map(|row| row.skipped.as_ref().map(|skip| (*row, skip)))
        .min_by_key(|(row, _)| {
            let order = releases
                .iter()
                .find(|release| release.version.as_str() == row.to)
                .map(|release| &release.order);
            (order.is_none(), order)
        })
}

/// Whether `graph` shows `dep`'s copy moved onto `destination` at `to`.
///
/// A declared copy has moved once every member declaring it binds the destination and no longer
/// binds the copy, as a uv fork pin can keep a member on both.
/// A copy no member declares has moved once the destination is in the graph and the copy is
/// not, which under-claims a Go module `go mod tidy` keeps as another module's requirement.
fn copy_landed(
    graph: &[Dependency],
    dep: &Dependency,
    destination: &PackageId,
    to: &Version,
) -> bool {
    let declared = dep.direct && !dep.members.is_empty();
    line_reached(
        graph,
        (&dep.package, &dep.current),
        (destination, to),
        declared.then_some(dep.members.as_slice()),
    ) && (declared
        || !graph
            .iter()
            .any(|node| node.package == dep.package && node.current == dep.current))
}

#[cfg(test)]
mod tests {
    use super::{CopyOutcome, PlannedCopy, Provenance, settle};
    use crate::app::upgrade::executor::planning::Judged;
    use crate::app::{FetchedRelease, SkippedInfo, UpgradeItem};
    use cooldown_core::{
        Change, Dependency, Diagnostic, DiagnosticKind, MajorKey, MemberRef, PackageId, Release,
        ReleaseOrder, ReleaseQuality, SkipReason, Status, ToolId, UpdateKind, Verdict, Version,
    };

    fn member(name: &str) -> MemberRef {
        MemberRef {
            name: name.to_string(),
            path: name.to_string(),
        }
    }

    /// `name@version`, declared by `members` when `direct`.
    fn node(name: &str, version: &str, direct: bool, members: &[&str]) -> Dependency {
        Dependency {
            package: PackageId::new(ToolId("npm"), name, None),
            advisory_identity: None,
            current: Version::new(version),
            current_quality: ReleaseQuality::Stable,
            direct,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: members.iter().map(|name| member(name)).collect(),
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    /// The first plan's copy of `dep`, whose releases are `versions` in ascending order, with
    /// each version's major key.
    fn fact(dep: Dependency, versions: &[(&str, &str)]) -> PlannedCopy {
        let releases = versions
            .iter()
            .zip(0_u8..)
            .map(|((version, major), order)| Release {
                version: Version::new(*version),
                order: ReleaseOrder(vec![order]),
                major: MajorKey((*major).to_string()),
                major_number: None,
                kind_from_current: None,
                beyond_declared_bound: false,
                beyond_latest_tag: false,
                published_at: None,
                yanked: false,
                quality: ReleaseQuality::Stable,
            })
            .collect();
        PlannedCopy {
            fact: FetchedRelease {
                dependency: dep,
                result: Ok(Judged {
                    releases,
                    verdict: Verdict {
                        status: Status::Adoptable,
                        adoptable_target: None,
                        latest: None,
                        candidates: Vec::new(),
                        held_reason: None,
                    },
                }),
            },
            outcome: None,
        }
    }

    /// The change moving `dep`'s copy to `to`, under the package `name`.
    fn change(dep: &Dependency, name: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(ToolId("npm"), name, None),
            from: dep.current.clone(),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: dep.direct,
            members: dep.members.clone(),
        }
    }

    /// The row reporting `change`, applied unless `held`, carrying `members` as its own.
    fn row(change: &Change, members: &[&str], held: bool) -> UpgradeItem {
        UpgradeItem {
            name: change.package.name.clone(),
            tool: "npm".to_string(),
            project: ".".to_string(),
            direct: change.direct,
            downgrade: false,
            members: members.iter().map(|name| member(name)).collect(),
            registry: None,
            from: change.from.to_string(),
            to: change.to.to_string(),
            kind: UpdateKind::Minor,
            applied: !held,
            skipped: held.then(|| SkippedInfo {
                reason: SkipReason::ResolverConflict,
                message: "held".to_string(),
                offending: None,
            }),
            error: None,
            edge: None,
            security: None,
        }
    }

    /// A collateral row moving `dep`'s copy to `to`, unattributed as a lock diff reports it, and
    /// listing the `members` the graph after it shows there.
    fn collateral(dep: &Dependency, to: &str, members: &[&str]) -> UpgradeItem {
        let mut moved = change(dep, &dep.package.name, to);
        moved.direct = false;
        row(&moved, members, false)
    }

    fn landed(to: &str) -> CopyOutcome {
        CopyOutcome::Landed {
            to: to.to_string(),
            security: None,
        }
    }

    /// What [`settle`] makes of each of `copies`, given what the pass recorded.
    fn settled(
        mut copies: Vec<PlannedCopy>,
        provenance: &Provenance,
        items: &[UpgradeItem],
        errors: &[Diagnostic],
        graph: Option<&[Dependency]>,
    ) -> Vec<CopyOutcome> {
        settle(&mut copies, provenance, items, errors, graph);
        copies
            .into_iter()
            .map(|copy| copy.outcome.unwrap_or(CopyOutcome::Undecided))
            .collect()
    }

    /// A planned move lands a declared copy only once every declaring member binds its target and
    /// no longer binds the copy: a split across two versions, or a member a fork pin keeps on
    /// both, lands nothing.
    #[test]
    fn a_declared_copy_lands_only_where_every_member_moved() {
        let dep = node("c", "1.0.0", true, &["web", "api"]);
        let facts = || {
            vec![fact(
                dep.clone(),
                &[("1.0.0", "1"), ("1.0.1", "1"), ("1.2.0", "1")],
            )]
        };
        let tried = change(&dep, "c", "1.2.0");
        let mut provenance = Provenance::default();
        provenance.tried(&tried, 0, &dep.package);
        let rows = [row(&tried, &["web", "api"], false)];

        // A split: `web` ends on another version.
        let split = settled(
            facts(),
            &provenance,
            &rows,
            &[],
            Some(&[
                node("c", "1.0.1", true, &["web"]),
                node("c", "1.2.0", true, &["api"]),
            ]),
        );
        assert_eq!(split, [CopyOutcome::Undecided]);

        // A fork pin: `api` reaches `1.2.0` and still binds `1.0.0`.
        let fork = settled(
            facts(),
            &provenance,
            &rows,
            &[],
            Some(&[
                node("c", "1.2.0", true, &["web", "api"]),
                node("c", "1.0.0", true, &["api"]),
            ]),
        );
        assert_eq!(fork, [CopyOutcome::Undecided]);

        let moved = settled(
            facts(),
            &provenance,
            &rows,
            &[],
            Some(&[node("c", "1.2.0", true, &["web", "api"])]),
        );
        assert_eq!(moved, [landed("1.2.0")]);
    }

    /// A planned move lands a copy no member declares only while the graph the pass left holds
    /// its target and not the copy: a source the graph keeps, or a later pass that dropped the
    /// package, lands nothing, and a final read that failed leaves the move to that failure.
    #[test]
    fn an_undeclared_copy_lands_while_the_graph_holds_its_target_alone() {
        let dep = node("t", "1.0.0", false, &[]);
        let facts = || vec![fact(dep.clone(), &[("1.0.0", "1"), ("1.1.0", "1")])];
        let tried = change(&dep, "t", "1.1.0");
        let mut provenance = Provenance::default();
        provenance.tried(&tried, 0, &dep.package);
        let rows = [row(&tried, &[], false)];
        let moved = node("t", "1.1.0", false, &[]);

        let gone = settled(
            facts(),
            &provenance,
            &rows,
            &[],
            Some(std::slice::from_ref(&moved)),
        );
        assert_eq!(gone, [landed("1.1.0")]);

        let kept = settled(
            facts(),
            &provenance,
            &rows,
            &[],
            Some(&[moved, dep.clone()]),
        );
        assert_eq!(kept, [CopyOutcome::Undecided]);

        let dropped = settled(facts(), &provenance, &rows, &[], Some(&[]));
        assert_eq!(dropped, [CopyOutcome::Undecided]);

        let unread = Diagnostic::new(DiagnosticKind::ToolFailed, "the read failed");
        let unconfirmed = settled(
            facts(),
            &provenance,
            &rows,
            std::slice::from_ref(&unread),
            None,
        );
        assert_eq!(unconfirmed, [CopyOutcome::Failed(unread)]);
    }

    /// Another package's move never lands a copy, even where the graph shows the copy moved
    /// there, because no row proves the copy's own bindings went there: such a copy stays
    /// undecided, or refused at the target its own move was held.
    /// A refusal names its target and reason only, so a graph that holds that target, as when
    /// another copy already sat there, never turns it into a landing.
    #[test]
    fn a_move_of_another_package_never_lands_a_copy() {
        let dep = node("leaf", "1.0.0", false, &[]);
        let facts = || {
            vec![fact(
                dep.clone(),
                &[("1.0.0", "1"), ("1.0.1", "1"), ("1.2.0", "1")],
            )]
        };
        let carried = settled(
            facts(),
            &Provenance::default(),
            &[collateral(&dep, "1.0.1", &[])],
            &[],
            Some(&[node("leaf", "1.0.1", false, &[])]),
        );
        assert_eq!(carried, [CopyOutcome::Undecided]);

        let tried = change(&dep, "leaf", "1.2.0");
        let mut provenance = Provenance::default();
        provenance.tried(&tried, 0, &dep.package);
        for rows in [
            vec![row(&tried, &[], true)],
            vec![row(&tried, &[], true), collateral(&dep, "1.0.1", &[])],
        ] {
            let outcomes = settled(
                facts(),
                &provenance,
                &rows,
                &[],
                Some(&[
                    node("leaf", "1.2.0", false, &[]),
                    node("leaf", "1.0.1", false, &[]),
                ]),
            );
            assert!(
                matches!(outcomes.as_slice(), [CopyOutcome::Refused { to, .. }] if to == "1.2.0"),
                "{outcomes:?}"
            );
        }
    }

    /// A Go move onto another major is planned under its target's module path and held under
    /// the path the graph still requires, and both trace back to the copy.
    #[test]
    fn a_go_move_traces_under_either_module_path() {
        let dep = node("example.com/m", "v1.0.0", true, &[]);
        let versions = [("v1.0.0", ""), ("v2.0.0", "/v2")];
        let tried = change(&dep, "example.com/m/v2", "v2.0.0");
        let mut provenance = Provenance::default();
        provenance.tried(&tried, 0, &dep.package);

        let landing = settled(
            vec![fact(dep.clone(), &versions)],
            &provenance,
            &[row(&tried, &[], false)],
            &[],
            Some(&[node("example.com/m/v2", "v2.0.0", true, &[])]),
        );
        assert_eq!(landing, [landed("v2.0.0")]);

        let reported = change(&dep, "example.com/m", "v2.0.0");
        let hold = settled(
            vec![fact(dep.clone(), &versions)],
            &provenance,
            &[row(&reported, &[], true)],
            &[],
            None,
        );
        assert!(
            matches!(hold.as_slice(), [CopyOutcome::Refused { to, .. }] if to == "v2.0.0"),
            "{hold:?}"
        );
    }

    /// A failure settles a copy the pass met it reading, or one it stopped before deciding the
    /// copy it tried or deferred; a failure that only names the copy's package, and one that
    /// left a copy the pass never meant to move, settle nothing.
    #[test]
    fn a_failure_settles_only_the_copies_it_left_undecided() {
        let tried = node("tried", "1.0.0", true, &[]);
        let deferred = node("deferred", "1.0.0", false, &[]);
        let untouched = node("untouched", "1.0.0", true, &[]);
        let unread = node("unread", "1.0.0", true, &[]);
        let versions = [("1.0.0", "1"), ("1.1.0", "1")];
        let facts = || {
            vec![
                fact(tried.clone(), &versions),
                fact(deferred.clone(), &versions),
                fact(untouched.clone(), &versions),
                fact(unread.clone(), &versions),
            ]
        };
        let mut provenance = Provenance::default();
        provenance.tried(&change(&tried, "tried", "1.1.0"), 0, &tried.package);
        provenance.await_replan(vec![1]);
        let fetch =
            Diagnostic::new(DiagnosticKind::ToolFailed, "no metadata").with_package("unread");
        provenance.failed(3, fetch.clone());
        let stopped = Diagnostic::new(DiagnosticKind::ToolFailed, "the read failed");
        let named =
            Diagnostic::new(DiagnosticKind::ToolFailed, "no metadata").with_package("untouched");

        let outcomes = settled(
            facts(),
            &provenance,
            &[],
            &[stopped.clone(), fetch.clone(), named],
            None,
        );

        assert_eq!(
            outcomes,
            [
                CopyOutcome::Failed(stopped.clone()),
                CopyOutcome::Failed(stopped),
                CopyOutcome::Undecided,
                CopyOutcome::Failed(fetch),
            ]
        );

        // A follower replan that read the graph decided the deferred copy itself.
        provenance.replanned();
        let replanned = settled(facts(), &provenance, &[], &[], None);
        assert_eq!(replanned.get(1), Some(&CopyOutcome::Undecided));
    }
}
