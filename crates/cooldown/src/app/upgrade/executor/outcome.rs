//! What an `upgrade` pass settled for each dependency copy its first plan read, which `outdated`
//! reports.
//!
//! The pass's rows are events: a planned move, a hold, a lock entry another move carried.
//! A report row is a claim about state, what the copy's declaring members bind once the pass is
//! over, so the copies are settled here, where the graph the pass left can prove a landing.

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
/// That graph is the only proof of a landing, and a copy lands only at the one version it
/// confirms for the copy among those its rows moved, or meant to move, it to (see
/// [`copy_landed`]).
/// A move that reaches only some of a declared copy's members, splits the copy across versions,
/// or was undone by a later pass therefore never reads as landed.
///
/// For each copy the first outcome that holds wins:
///
/// - **Landed** at that one version.
/// - **Refused** by the hold of the lowest target a plan tried, whatever the reason.
/// - **Failed** by the error of such a move, an error naming the package, or the failure that
///   stopped the pass before it confirmed a move of the copy or decided a copy it tried or
///   deferred.
/// - **Undecided** otherwise.
pub(super) fn settle(
    copies: &mut [PlannedCopy],
    provenance: &Provenance,
    items: &[UpgradeItem],
    errors: &[Diagnostic],
    graph: Option<&[Dependency]>,
) {
    let mut traced: HashMap<usize, Vec<&UpgradeItem>> = HashMap::new();
    let mut by_source: HashMap<(&str, Option<&str>, &str), Vec<&UpgradeItem>> = HashMap::new();
    for item in items.iter().filter(|item| item.edge.is_none()) {
        if let Some(&copy) = provenance.origin.get(&ChangeKey::of_item(item)) {
            traced.entry(copy).or_default().push(item);
        }
        if item.applied && !item.downgrade {
            by_source
                .entry((
                    item.name.as_str(),
                    item.registry.as_deref(),
                    item.from.as_str(),
                ))
                .or_default()
                .push(item);
        }
    }
    let tried: HashSet<usize> = provenance.origin.values().copied().collect();
    let failure = errors.iter().rev().find(|error| error.package.is_none());
    for (index, copy) in copies.iter_mut().enumerate() {
        let dep = &copy.fact.dependency;
        let rows = traced.get(&index).map(Vec::as_slice).unwrap_or_default();
        let source = (
            dep.package.name.as_str(),
            dep.package.registry.as_deref(),
            dep.current.as_str(),
        );
        let forward = by_source
            .get(&source)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let landing = graph.and_then(|graph| single_landing(graph, dep, rows, forward));
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
        } else if let Some(error) = rows.iter().find_map(|row| row.error.as_ref()).or_else(|| {
            errors
                .iter()
                .find(|error| error.package.as_ref() == Some(&dep.package.name))
        }) {
            CopyOutcome::Failed(error.clone())
        } else {
            // A copy the pass tried or deferred and never settled, or one it moved where the
            // final read could not confirm the move, is left to the failure that stopped it.
            let pending = tried.contains(&index)
                || provenance.awaiting.contains(&index)
                || (graph.is_none() && !forward.is_empty());
            match failure {
                Some(failure) if pending => CopyOutcome::Failed(failure.clone()),
                _ => CopyOutcome::Undecided,
            }
        };
        copy.outcome = Some(outcome);
    }
}

/// The one row whose version `graph` confirms for `dep`'s copy among every version its traced
/// `rows` and the `forward` rows from its version took, or meant to take, it to, or `None` when
/// it confirms none or several.
///
/// Applied rows come first, so a landing reports their evidence.
fn single_landing<'a>(
    graph: &[Dependency],
    dep: &Dependency,
    rows: &[&'a UpgradeItem],
    forward: &[&'a UpgradeItem],
) -> Option<&'a UpgradeItem> {
    let candidates = rows
        .iter()
        .chain(forward)
        .filter(|row| row.applied)
        .chain(rows.iter().filter(|row| !row.applied))
        .copied()
        .filter(|row| !row.downgrade);
    let mut landings: Vec<&UpgradeItem> = Vec::new();
    for row in candidates {
        let seen = landings.iter().any(|landing| {
            landing.name == row.name && landing.registry == row.registry && landing.to == row.to
        });
        // A Go move onto another major lands under that major's module path.
        let destination = PackageId::new(dep.package.tool, row.name.clone(), row.registry.clone());
        if !seen && copy_landed(graph, dep, &destination, &Version::new(row.to.clone())) {
            landings.push(row);
        }
    }
    match landings.as_slice() {
        [row] => Some(row),
        _ => None,
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
/// not.
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

    /// A collateral move counts for a declared copy only once every declaring member binds its
    /// version and no longer binds the copy: a split across two versions, or a member a fork pin
    /// keeps on both, lands nothing and leaves the tried target's hold.
    #[test]
    fn a_declared_copy_lands_only_where_every_member_moved() {
        let dep = node("c", "1.0.0", true, &["web", "api"]);
        let versions = [("1.0.0", "1"), ("1.0.1", "1"), ("1.2.0", "1")];
        let tried = change(&dep, "c", "1.2.0");
        let mut provenance = Provenance::default();
        provenance.tried(&tried, 0, &dep.package);
        let held = row(&tried, &["web", "api"], true);

        // A split: each member's edge moved to another version.
        let split = settled(
            vec![fact(dep.clone(), &versions)],
            &provenance,
            &[
                collateral(&dep, "1.0.1", &["web"]),
                collateral(&dep, "1.2.0", &["api"]),
                held.clone(),
            ],
            &[],
            Some(&[
                node("c", "1.0.1", true, &["web"]),
                node("c", "1.2.0", true, &["api"]),
            ]),
        );
        assert!(
            matches!(split.as_slice(), [CopyOutcome::Refused { to, .. }] if to == "1.2.0"),
            "{split:?}"
        );

        // A fork pin: `api` reaches `1.2.0` and still binds `1.0.0`.
        let fork = settled(
            vec![fact(dep.clone(), &versions)],
            &Provenance::default(),
            &[collateral(&dep, "1.2.0", &["web", "api"])],
            &[],
            Some(&[
                node("c", "1.2.0", true, &["web", "api"]),
                node("c", "1.0.0", true, &["api"]),
            ]),
        );
        assert_eq!(fork, [CopyOutcome::Undecided]);

        // Every member moved.
        let moved = settled(
            vec![fact(dep.clone(), &versions)],
            &provenance,
            &[collateral(&dep, "1.2.0", &["web", "api"]), held],
            &[],
            Some(&[node("c", "1.2.0", true, &["web", "api"])]),
        );
        assert_eq!(moved, [landed("1.2.0")]);
    }

    /// A copy no member declares lands at the version it moved to once the graph holds that
    /// version and no longer holds the copy, and never while it still does, when its moves
    /// reached several versions, or when the graph could not be read.
    #[test]
    fn an_undeclared_copy_lands_at_one_confirmed_version() {
        let dep = node("child", "1.0.0", false, &[]);
        let facts = || {
            vec![fact(
                dep.clone(),
                &[("1.0.0", "1"), ("1.1.0", "1"), ("1.2.0", "1")],
            )]
        };
        let rows = [collateral(&dep, "1.1.0", &[])];
        let moved = [node("child", "1.1.0", false, &[])];
        let gone = settled(facts(), &Provenance::default(), &rows, &[], Some(&moved));
        assert_eq!(gone, [landed("1.1.0")]);

        let kept = [moved[0].clone(), dep.clone()];
        let kept = settled(facts(), &Provenance::default(), &rows, &[], Some(&kept));
        assert_eq!(kept, [CopyOutcome::Undecided]);

        let unread = settled(facts(), &Provenance::default(), &rows, &[], None);
        assert_eq!(unread, [CopyOutcome::Undecided]);

        // Two destinations coexist, so neither is the copy's landing.
        let split = [
            collateral(&dep, "1.1.0", &[]),
            collateral(&dep, "1.2.0", &[]),
        ];
        let both = [
            node("child", "1.1.0", false, &[]),
            node("child", "1.2.0", false, &[]),
        ];
        let split = settled(facts(), &Provenance::default(), &split, &[], Some(&both));
        assert_eq!(split, [CopyOutcome::Undecided]);
    }

    /// A pnpm split moves one requirer's copy to the planned target and another's elsewhere while
    /// the settlement holds the planned target, so the copy has not landed and the hold stands.
    #[test]
    fn a_pnpm_split_keeps_the_hold() {
        let dep = node("leaf", "1.0.0", false, &[]);
        let facts = || {
            vec![fact(
                dep.clone(),
                &[("1.0.0", "1"), ("1.0.1", "1"), ("1.2.0", "1")],
            )]
        };
        let tried = change(&dep, "leaf", "1.2.0");
        let mut provenance = Provenance::default();
        provenance.tried(&tried, 0, &dep.package);

        let outcomes = settled(
            facts(),
            &provenance,
            &[row(&tried, &[], true), collateral(&dep, "1.0.1", &[])],
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

    /// A planned move the pass landed counts only while the graph it left still holds the
    /// target: a later pass that dropped the package undoes it.
    #[test]
    fn a_planned_landing_a_later_pass_undid_has_not_landed() {
        let dep = node("t", "1.0.0", false, &[]);
        let facts = || vec![fact(dep.clone(), &[("1.0.0", "1"), ("1.1.0", "1")])];
        let tried = change(&dep, "t", "1.1.0");
        let mut provenance = Provenance::default();
        provenance.tried(&tried, 0, &dep.package);
        let rows = [row(&tried, &[], false)];

        let kept = settled(
            facts(),
            &provenance,
            &rows,
            &[],
            Some(&[node("t", "1.1.0", false, &[])]),
        );
        assert_eq!(kept, [landed("1.1.0")]);

        let dropped = settled(facts(), &provenance, &rows, &[], Some(&[]));
        assert_eq!(dropped, [CopyOutcome::Undecided]);

        // A final read that failed leaves the move to that failure.
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

    /// A failure settles a copy when it names the copy's package, or when it stopped the pass
    /// before it decided a copy it tried or deferred; another package's failure, and one that
    /// left a copy the pass never meant to move, settle nothing.
    #[test]
    fn a_failure_settles_only_the_copies_it_left_undecided() {
        let tried = node("tried", "1.0.0", true, &[]);
        let deferred = node("deferred", "1.0.0", false, &[]);
        let untouched = node("untouched", "1.0.0", true, &[]);
        let named = node("named", "1.0.0", true, &[]);
        let versions = [("1.0.0", "1"), ("1.1.0", "1")];
        let facts = || {
            vec![
                fact(tried.clone(), &versions),
                fact(deferred.clone(), &versions),
                fact(untouched.clone(), &versions),
                fact(named.clone(), &versions),
            ]
        };
        let mut provenance = Provenance::default();
        provenance.tried(&change(&tried, "tried", "1.1.0"), 0, &tried.package);
        provenance.await_replan(vec![1]);
        let stopped = Diagnostic::new(DiagnosticKind::ToolFailed, "the read failed");
        let fetch =
            Diagnostic::new(DiagnosticKind::ToolFailed, "no metadata").with_package("named");
        let other =
            Diagnostic::new(DiagnosticKind::ToolFailed, "no metadata").with_package("elsewhere");

        let outcomes = settled(
            facts(),
            &provenance,
            &[],
            &[stopped.clone(), fetch.clone(), other],
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
