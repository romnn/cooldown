//! Peer-feasibility evidence, policy gates, and post-resolve verification.

use crate::apply::landing::restore_after_owned_step;
use crate::lock::{NameVersion, NodeLock};
use crate::registry::NPM;
use crate::version::{self, RangeMatch};
use camino::Utf8Path;
use cooldown_core::{
    ApplyReport, Change, CoreError, MemberRef, PackageId, Plan, Project, ProjectMutationFile,
    ProjectMutationJournal, Result, SkipReason, Skipped,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// The journaled pre-apply lockfile body, when it was captured and is valid UTF-8.
pub(crate) fn journaled_lock<L: NodeLock>(journal: &ProjectMutationJournal) -> Option<&str> {
    journal
        .files()
        .iter()
        .find(|file| file.path() == Utf8Path::new(L::LOCKFILE))
        .and_then(ProjectMutationFile::contents)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
}

/// Everything the peer-feasibility gate reads from one immutable pre-apply lock, gathered once
/// per apply so no evidence source is parsed twice (the member index in particular is shared
/// between importer attribution and the workspace-manifest peer source).
pub(crate) struct PeerEvidence {
    /// Peer contracts recorded in the lock's resolved-package entries.
    requirements: Vec<crate::lock::PeerRequirement>,
    /// Importer attribution: which member declares which `(name, version)`.
    members: crate::lock::MemberIndex,
    /// Names *declared* at multiple versions across importers.
    multi_version: HashSet<String>,
    /// Names *resolved* at multiple versions anywhere in the graph.
    resolved_splits: HashSet<String>,
    /// Peer contracts read from workspace member manifests (empty without a project root).
    pub(crate) workspace: Vec<WorkspacePeer>,
    /// The physical install layout, when the lock records one (npm).
    /// Peer visibility is then
    /// judged by nearest-ancestor lookup instead of declaring-member overlap: hoisting lets
    /// disjoint members' packages meet at the root `node_modules`.
    install: Option<crate::lock::InstallPaths>,
}

impl PeerEvidence {
    /// Gathers the gate's evidence from the journaled pre-apply lock.
    /// `root` locates the member
    /// manifests for the workspace source; `None` (lock-only contexts) leaves that source empty.
    /// The multi-version sets are computed only when some contract could hold, so the common
    /// no-peers apply skips those lock walks entirely.
    pub(crate) fn gather<L: NodeLock>(root: Option<&Utf8Path>, lock: Option<&str>) -> Self {
        let requirements = lock.map(L::peer_requirements).unwrap_or_default();
        let members = L::member_sources(lock.unwrap_or_default());
        let workspace = match (root, lock) {
            (Some(root), Some(lock)) => workspace_peer_requirements::<L>(root, lock, &members),
            _ => Vec::new(),
        };
        let (multi_version, resolved_splits, install) =
            if requirements.is_empty() && workspace.is_empty() {
                (HashSet::new(), HashSet::new(), None)
            } else {
                (
                    members.names_declared_at_multiple_versions(),
                    resolved_multi_version_names::<L>(lock.unwrap_or_default()),
                    lock.and_then(L::install_paths),
                )
            };
        Self {
            requirements,
            members,
            multi_version,
            resolved_splits,
            workspace,
            install,
        }
    }
}

/// The peer-feasibility gate's split of a plan (see [`partition_peer_held`]).
pub(crate) struct PeerPartition {
    /// The plan with only the changes the resolve may attempt; every other plan setting passes
    /// through untouched.
    pub(crate) retained: Plan,
    /// One [`SkipReason::PeerHeld`] row per held change, naming the blocking dependent and its
    /// verbatim range.
    pub(crate) skipped: Vec<Skipped>,
}

/// The peer-feasibility gate: split the plan into the changes the resolve may attempt and the
/// cross-major changes a lock-recorded peer requirement structurally excludes, each held as a
/// [`SkipReason::PeerHeld`] naming the dependent and its verbatim range.
///
/// pnpm only *warns* on a peer mismatch by default, and npm — which rejects with `ERESOLVE` by
/// default — commits it under relaxed enforcement (`legacy-peer-deps`, common in project
/// `.npmrc`s), so without this gate a cross-major move that breaks a still-present dependent's
/// peer contract (`fumadocs-core` 16→17 under `fumadocs-mdx`'s `fumadocs-core@^16`) can resolve
/// "successfully" and land the break silently.
/// Holding it up front makes `upgrade` skip it with
/// the range named, and `outdated`'s verification reclassify the row `blocked by <dependent>` —
/// the two commands agree.
///
/// The gate only fires when the violation is demonstrable, and fails open everywhere else:
///
/// - only forward moves across an npm compatibility line are gated — the same
///   [`major_key`](version::major_key) predicate that gates `--major`, so `0.1 → 0.2` counts
///   (caret semantics make the 0.x minor the breaking axis).
///   The line is judged from the versions,
///   never the caller-supplied kind, which `outdated`'s verification passes neutrally; in-range
///   moves are the resolver's business;
/// - the range must demonstrably bind: it *matches* the current version and *provably excludes*
///   the target, judged with the tri-state [`range_match`](version::range_match) (peer ranges
///   routinely union majors — `^7.0.0 || ^8.0.0`).
///   A range with any branch the matcher cannot
///   represent (npm hyphen ranges, `workspace:*`) yields `Unknown`, never `Excludes`, so it cannot
///   block;
/// - only an importer-declared (direct) dependent gates.
///   A *transitive* dependent can be floated by
///   the resolver within its parents' ranges to a sibling version that admits the target (npm does
///   exactly this when a peer conflict arises), so its lock-recorded peer range is not
///   authoritative — and a float that would need a still-cooling version is already stopped by the
///   transitive cooldown gate.
///   A direct dependent has no such latitude: an in-range version that
///   lifts the peer would itself have been planned as a move (and then the co-move rule applies);
/// - npm's package-lock attributes importer declarations by *name* only, but its physical layout
///   is instance-exact: a declaring member's own nearest-ancestor lookup identifies its direct
///   copy ([`direct_dependent_members`]), so a name resolved at several versions still gates
///   through the proven-direct instance — and only through it.
///   Without a physical layout (an npm
///   v1 lock) the split stays ambiguous and never gates (pnpm's attribution is version-exact and
///   unaffected);
/// - on a manager with a whole-graph resolve (pnpm), the dependent must not itself be moving in
///   the dependent's own importing context — its target may lift the peer range, and the joint
///   resolve settles the pair's peer contexts in one pass.
///   Holds are recomputed to a fixed point,
///   so a dependent that is itself peer-held (or destined for the resolve's own multi-version
///   skip) stops exempting the packages it pins.
///   Two co-moving packages that each block the
///   other's old range are deliberately both exempt: that joint move is exactly what the joint
///   resolve exists to judge.
///   A manager on the sequential per-package path (npm) grants NO co-move
///   exemption: its resolver never judges the pair jointly — the moves land one at a time, so an
///   exempted package could land against the dependent's *old* range with only a warning.
///   The package stays held while the dependent moves; the dependent's own landing is post-verified
///   ([`settle_landed_candidate`]) and rolled back when its *new* range provably breaks against
///   the still-held package, so a strict lockstep pair stays held on both sides (moved together
///   manually, e.g. `npm install react@19 react-dom@19`) instead of committing a broken
///   intermediate; a dependent whose new range still admits the current version lands, and the
///   next run releases the hold;
/// - peer visibility follows the layout the manager actually resolves against. pnpm importers are
///   isolated by declaration, so a dependent whose declaring importers are disjoint from the
///   change's members keeps its own in-range copy and the moved copy cannot break it. npm's
///   default layout *hoists* — packages declared by disjoint members meet at the root
///   `node_modules` — so there the lock's physical paths decide ([`InstallPaths`]): a dependent
///   whose nearest-ancestor lookup reaches the moving copy is bound wherever it is declared,
///   while one shadowed by its own nested copy is not;
/// - a name declared at multiple versions across the workspace is left to the resolve's own
///   [`MultiVersionHeld`](SkipReason::MultiVersionHeld) classification;
/// - peer contracts come from TWO sources: the lock's resolved-package records
///   ([`NodeLock::peer_requirements`]) and every workspace member's own `package.json`
///   ([`workspace_peer_requirements`], gathered into [`PeerEvidence`]) — the lock is not
///   authoritative for a local package's peers, which pnpm keeps only in the manifest whether
///   the package is symlinked (`link:`) or injected (`file:`).
///   A workspace-local dependent binds
///   in its own directory and in every importer that consumes or declares it, and it never moves
///   in a plan, so no co-move exemption applies — only editing its peer range lifts the hold.
///   Cooldown never edits it: a `peerDependencies` entry is a contract the package publishes to
///   its consumers, not a declaration of what it installs, so manifest widening leaves that field
///   alone entirely (`WIDENABLE_FIELDS`) and the workspace-manifest hold covers every provable
///   break rather than only cross-line ones ([`workspace_peer_hold`]).
///   Together those make the
///   contract immutable for the whole apply, which is what lets the pre-apply snapshot serve as
///   the post-resolve verifier's evidence without going stale.
pub(crate) fn partition_peer_held<L: NodeLock>(
    plan: &Plan,
    evidence: &PeerEvidence,
) -> PeerPartition {
    let PeerEvidence {
        requirements,
        members,
        multi_version,
        resolved_splits,
        workspace,
        install,
    } = evidence;
    if requirements.is_empty() && workspace.is_empty() {
        return PeerPartition {
            retained: plan.clone(),
            skipped: Vec::new(),
        };
    }

    // One verdict slot per change, filled to a fixed point: a change held in one round stops
    // moving, which can only expose further holds (never lift one), so the rounds are monotone,
    // order-independent in their result, and bounded by the plan length.
    let mut verdicts: Vec<Option<&crate::lock::PeerRequirement>> = vec![None; plan.changes.len()];
    loop {
        // The changes whose own move may exempt their dependents this round.
        // Only a manager with a
        // whole-graph resolve gets the exemption at all — the sequential per-package path never
        // judges a pair jointly, so there `moving` stays empty and every binding range holds.
        // One
        // destined for the resolve's own multi-version skip never counts as moving either: it
        // stays in place, so it must not exempt its dependents.
        let moving: Vec<&Change> = if L::supports_whole_graph_resolve() {
            plan.changes
                .iter()
                .zip(&verdicts)
                .filter(|(change, verdict)| {
                    verdict.is_none() && !multi_version.contains(&change.package.name)
                })
                .map(|(change, _)| change)
                .collect()
        } else {
            Vec::new()
        };
        let mut grew = false;
        for (change, verdict) in plan.changes.iter().zip(verdicts.iter_mut()) {
            if verdict.is_none()
                && let Some(blocker) = peer_hold(
                    change,
                    requirements,
                    members,
                    multi_version,
                    resolved_splits,
                    &moving,
                    install.as_ref(),
                )
                .or_else(|| workspace_peer_hold(change, workspace, multi_version, install.as_ref()))
            {
                *verdict = Some(blocker);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    let mut retained = Vec::with_capacity(plan.changes.len());
    let mut skipped = Vec::new();
    for (change, verdict) in plan.changes.iter().zip(&verdicts) {
        match verdict {
            Some(blocker) => {
                skipped.push(peer_held_skip::<L>(
                    change,
                    blocker,
                    blocker.dependent.clone(),
                ));
            }
            None => retained.push(change.clone()),
        }
    }
    // Rebuild with only the retained changes; every other plan setting passes through untouched
    // (including fields this adapter has no use for, like the cargo edge policy).
    let retained = Plan {
        changes: retained,
        ..plan.clone()
    };
    PeerPartition { retained, skipped }
}

/// The `peer_held` skip row for `change`, blocked by `blocker`'s recorded contract.
/// `offending`
/// names the party whose move (or range edit) would release the hold: the dependent when the
/// change is the peer target, the peer package when the change is the dependent itself (the
/// post-apply verification path).
/// One constructor keeps the two paths from drifting in wording.
pub(crate) fn peer_held_skip<L: NodeLock>(
    change: &Change,
    blocker: &crate::lock::PeerRequirement,
    offending: String,
) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::PeerHeld,
        offending: Some(PackageId::new(L::ID, offending, Some(NPM.to_string()))),
        detail: Some(format!(
            "held: {}@{} requires {}@{}",
            blocker.dependent, blocker.dependent_version, blocker.package, blocker.range
        )),
    }
}

/// Settles a sequential candidate whose requested version landed: the landing alone is not
/// sufficient, because under relaxed peer enforcement (`legacy-peer-deps` in a project `.npmrc`)
/// npm commits a graph that provably breaks a peer contract — reachable here when a held pair's
/// dependent is applied alone and its *new* peer range admits only the held target's new major.
/// A candidate whose lock introduces a violation absent from `baseline` is restored from its
/// journal and reported `peer_held`, blaming the contract's *other* party (the one whose move or
/// range edit releases the hold) — the break the pre-apply gate exists to prevent must not
/// persist even between runs.
///
/// `baseline` carries the violations of the last *accepted* lock across the candidate loop
/// (initialized lazily from the first settled candidate's journal snapshot): an accepted
/// candidate's violation map becomes the next baseline, and a rolled-back candidate restores the
/// disk to exactly the baseline state, so each candidate lock is parsed once.
pub(crate) fn settle_landed_candidate<L: NodeLock>(
    project: &Project,
    change: &Change,
    candidate_journal: &ProjectMutationJournal,
    candidate_postimage: &cooldown_core::ProjectMutationState,
    workspace: &[WorkspacePeer],
    baseline: &mut Option<PeerViolations>,
    report: &mut ApplyReport,
) -> Result<()> {
    let after = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
    let current = proven_peer_violations::<L>(&after, workspace);
    let fresh = {
        let base = baseline.get_or_insert_with(|| {
            journaled_lock::<L>(candidate_journal)
                .map(|lock| proven_peer_violations::<L>(lock, workspace))
                .unwrap_or_default()
        });
        current
            .iter()
            .find(|(id, detail)| !baseline_covers(base, id, detail))
            .map(|(id, _)| id.requirement())
    };
    match fresh {
        None => {
            *baseline = Some(current);
            report.applied.push(change.clone());
        }
        Some(violation) => {
            restore_after_owned_step(candidate_journal, candidate_postimage)?;
            let offending = if violation.dependent == change.package.name {
                violation.package.clone()
            } else {
                violation.dependent.clone()
            };
            report
                .skipped
                .push(peer_held_skip::<L>(change, &violation, offending));
        }
    }
    Ok(())
}

/// Identifies one proven violation *instance*: the dependent at its exact landed version, the
/// violated peer package, and the recorded range.
/// The dependent's version is part of the identity
/// deliberately — two same-named dependent copies (a split held at 1.x in one importer and
/// floated to 3.x in another, both recording the same range) are distinct instances whose
/// violations must not collapse onto one key, or the float's fresh break inherits the old break's
/// grandfathering ([`baseline_covers`] handles the version-insensitive part of "already broken").
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PeerViolationId {
    pub(crate) dependent: String,
    dependent_version: String,
    pub(crate) package: String,
    pub(crate) range: String,
}

impl PeerViolationId {
    /// The violation as the reporting-side [`PeerRequirement`] (the blame row's contract line).
    fn requirement(&self) -> crate::lock::PeerRequirement {
        crate::lock::PeerRequirement {
            dependent: self.dependent.clone(),
            dependent_version: self.dependent_version.clone(),
            package: self.package.clone(),
            range: self.range.clone(),
        }
    }
}

/// Proven peer violations of one lock, keyed by instance (a `BTreeMap`, so iteration — and thus
/// blame order — is deterministic).
pub(crate) type PeerViolations = BTreeMap<PeerViolationId, ViolationDetail>;

/// The contexts behind one proven violation: each binding member paired with the peer version its
/// own context demonstrably binds — resolved from the member's dependent instance on npm's
/// physical tree, or from the importer's own declaration on pnpm.
/// Coverage compares the members
/// ([`baseline_covers`]); attribution judges each binding counterfactually
/// ([`attribute_peer_violation`]).
#[derive(Clone)]
pub(crate) struct ViolationDetail {
    bindings: Vec<(String, String)>,
}

/// Whether the baseline already contained `fresh` in every context it binds: a violation of the
/// same contract *shape* — dependent, package, range, the dependent's exact version aside — with
/// a binding in each of the fresh members.
/// Version-insensitive on purpose (on both sides): a
/// dependent re-recorded at a new patch, or a peer floated between two versions the range
/// excludes either way, keeps an already-broken contract broken — nothing newly breaks, and
/// re-attributing it would reject an innocent candidate.
/// Member-sensitive on purpose: a
/// same-shaped violation surfacing in a member whose context was not violating before (a split
/// copy floated onto the broken range) is a NEW break, and grandfathering it would accept exactly
/// the break the verification exists to catch.
fn baseline_covers(
    baseline: &PeerViolations,
    fresh: &PeerViolationId,
    detail: &ViolationDetail,
) -> bool {
    detail
        .bindings
        .iter()
        .all(|(member, _)| member_covered(baseline, fresh, member))
}

/// The per-member half of [`baseline_covers`]: whether the baseline holds a same-shaped violation
/// binding in `member`.
fn member_covered(baseline: &PeerViolations, fresh: &PeerViolationId, member: &str) -> bool {
    baseline.iter().any(|(id, base)| {
        id.dependent == fresh.dependent
            && id.package == fresh.package
            && id.range == fresh.range
            && base.bindings.iter().any(|(bound, _)| bound == member)
    })
}

/// The immutable pre-apply peer facts the post-resolve verifier judges against, gathered once per
/// apply: the violations already present (never re-attributed to a candidate), the pre-apply
/// member/importer attribution and physical layout (the counterfactual "old peer version",
/// looked up per binding context), and each contract's uniquely recorded range (the
/// counterfactual "old range" — a dependent recorded at several distinct ranges yields no entry,
/// so attribution stays proof-only).
pub(crate) struct PeerBaseline {
    violations: PeerViolations,
    members: crate::lock::MemberIndex,
    install: Option<crate::lock::InstallPaths>,
    ranges: HashMap<(String, String), String>,
}

impl PeerBaseline {
    pub(crate) fn gather<L: NodeLock>(before: Option<&str>, workspace: &[WorkspacePeer]) -> Self {
        let content = before.unwrap_or_default();
        let mut unique: HashMap<(String, String), Option<String>> = HashMap::new();
        for pr in L::peer_requirements(content) {
            unique
                .entry((pr.dependent, pr.package))
                .and_modify(|slot| {
                    if slot.as_deref() != Some(pr.range.as_str()) {
                        *slot = None;
                    }
                })
                .or_insert(Some(pr.range));
        }
        PeerBaseline {
            violations: proven_peer_violations::<L>(content, workspace),
            members: L::member_sources(content),
            install: L::install_paths(content),
            ranges: unique
                .into_iter()
                .filter_map(|(key, range)| range.map(|range| (key, range)))
                .collect(),
        }
    }

    /// The version `member`'s context bound `name` at before the apply — the importer's own
    /// pre-apply declaration (pnpm), or the member's physical resolution on the pre-apply tree
    /// (npm).
    /// `None` when the context did not demonstrably bind it, which leaves the
    /// corresponding counterfactual unprovable.
    fn bound_before(&self, member: &str, name: &str) -> Option<&str> {
        self.members.resolved_version(member, name).or_else(|| {
            self.install
                .as_ref()
                .and_then(|install| install.member_resolution(member, name))
                .map(|instance| instance.version)
        })
    }
}

/// Which candidate one fresh binding proves culpable.
/// The after-lock only proves the *pair*
/// incompatible in that context, never which move broke it, so culpability is judged
/// counterfactually against the pre-apply baseline, per binding context:
///
/// - the dependent alone, when its landed range provably excludes the version this member's
///   context bound BEFORE — its move breaks even with the peer left in place;
/// - the peer alone, when the dependent's OLD recorded range provably excludes the version the
///   context binds now — its move breaks against the dependent as it already was;
/// - with only one side planned at all, that side (the other never moved);
/// - otherwise [`Unattributable`](PeerCulprit::Unattributable): both proofs hold, neither does,
///   or neither side is planned — rejecting would be a guess, so the caller propagates a
///   non-local rejection and candidate isolation decides.
enum PeerCulprit {
    Dependent,
    Peer,
    Unattributable,
}

fn attribute_peer_violation(
    baseline: &PeerBaseline,
    id: &PeerViolationId,
    member: &str,
    peer_version: &str,
    dependent_planned: bool,
    peer_planned: bool,
) -> PeerCulprit {
    match (dependent_planned, peer_planned) {
        (true, false) => return PeerCulprit::Dependent,
        (false, true) => return PeerCulprit::Peer,
        (false, false) => return PeerCulprit::Unattributable,
        (true, true) => {}
    }
    let dependent_breaks_old_peer = baseline
        .bound_before(member, &id.package)
        .is_some_and(|old| version::range_match(&id.range, old) == RangeMatch::Excludes);
    let old_range_breaks_new_peer = baseline
        .ranges
        .get(&(id.dependent.clone(), id.package.clone()))
        .is_some_and(|old_range| {
            version::range_match(old_range, peer_version) == RangeMatch::Excludes
        });
    match (dependent_breaks_old_peer, old_range_breaks_new_peer) {
        (true, false) => PeerCulprit::Dependent,
        (false, true) => PeerCulprit::Peer,
        _ => PeerCulprit::Unattributable,
    }
}

/// The immutable inputs one whole-graph apply hands its peer-verified resolve loop: the plan to
/// land, the pre-apply journal each round restores to, the multi-version names the resolve must not
/// pin, the per-candidate transitive-advance verdicts, the transitive age floor, and the
/// workspace-manifest peer contracts the lock is not authoritative for.
#[derive(Clone, Copy)]
pub(crate) struct JointResolve<'a> {
    pub(crate) plan: &'a Plan,
    pub(crate) journal: &'a ProjectMutationJournal,
    pub(crate) multi_version: &'a HashSet<String>,
    pub(crate) advance:
        &'a std::collections::HashMap<crate::tool::AdvanceKey, crate::tool::TransitiveAdvance>,
    pub(crate) window_minutes: Option<i64>,
    pub(crate) workspace: &'a [WorkspacePeer],
}

/// One candidate rejection a verification round decided: which active change to drop, the
/// violated contract, and the blamed other party.
pub(crate) struct PeerRejection {
    pub(crate) index: usize,
    pub(crate) violation: crate::lock::PeerRequirement,
    pub(crate) offending: String,
}

/// Plans this round's rejections from the fresh violations: every candidate a violation *uniquely*
/// proves culpable is rejected in one round — each proof is counterfactual against the immutable
/// baseline, so it stands regardless of the other rejections.
/// A violation touching an
/// already-rejected candidate is a cascade: that party reverts with the journal restore, so it is
/// re-judged after the re-resolve instead of guessed at now.
/// A violation binding in several
/// contexts must prove ONE candidate culpable across all of them; disagreement — like an
/// unattributable binding — aborts the round (`Err`), handing the interaction to the caller's
/// candidate isolation.
pub(crate) fn plan_peer_rejections(
    baseline: &PeerBaseline,
    current: &PeerViolations,
    active: &Plan,
    multi_version: &HashSet<String>,
) -> Result<Vec<PeerRejection>> {
    let mut rejections: Vec<PeerRejection> = Vec::new();
    for (id, detail) in current {
        let fresh: Vec<(&str, &str)> = detail
            .bindings
            .iter()
            .filter(|(member, _)| !member_covered(&baseline.violations, id, member))
            .map(|(member, peer_version)| (member.as_str(), peer_version.as_str()))
            .collect();
        if fresh.is_empty() {
            continue;
        }
        // A candidate is this violation's mover only when it actually landed the violating
        // instance: same name AND the exact landed version, moving in a fresh binding context —
        // name-only matching could blame a same-named change in an unrelated member.
        // A
        // multi-version name is no candidate at all: the whole-graph resolve deliberately never
        // pins it ([`prepare_whole_graph_inputs`]), so its landing is resolver latitude and
        // rejecting the unpinned change would alter nothing.
        let planned = |name: &str, landed: &str, contexts: &[&str]| -> Option<usize> {
            if multi_version.contains(name) {
                return None;
            }
            active.changes.iter().position(|change| {
                change.package.name == name
                    && change.to.as_str() == landed
                    && (change.members.is_empty()
                        || change
                            .members
                            .iter()
                            .any(|member| contexts.contains(&member.path.as_str())))
            })
        };
        let fresh_members: Vec<&str> = fresh.iter().map(|(member, _)| *member).collect();
        let dependent_index = planned(&id.dependent, &id.dependent_version, &fresh_members);
        let peer_indexes: Vec<Option<usize>> = fresh
            .iter()
            .map(|(member, peer_version)| {
                planned(&id.package, peer_version, std::slice::from_ref(member))
            })
            .collect();
        let already_rejected = |index: Option<usize>| {
            index.is_some_and(|index| rejections.iter().any(|rejection| rejection.index == index))
        };
        if already_rejected(dependent_index)
            || peer_indexes.iter().any(|index| already_rejected(*index))
        {
            continue;
        }
        let unattributable = || {
            CoreError::StaleLock(format!(
                "resolve broke a peer contract without a uniquely culpable candidate: \
                 {}@{} requires {}@{}",
                id.dependent, id.dependent_version, id.package, id.range
            ))
        };
        let mut agreed: Option<(usize, String)> = None;
        for ((member, peer_version), peer_index) in fresh.iter().zip(&peer_indexes) {
            let culprit = attribute_peer_violation(
                baseline,
                id,
                member,
                peer_version,
                dependent_index.is_some(),
                peer_index.is_some(),
            );
            let verdict = match culprit {
                PeerCulprit::Dependent => dependent_index.map(|index| (index, id.package.clone())),
                PeerCulprit::Peer => peer_index.map(|index| (index, id.dependent.clone())),
                PeerCulprit::Unattributable => None,
            };
            let Some(verdict) = verdict else {
                return Err(unattributable());
            };
            if agreed
                .as_ref()
                .is_some_and(|(index, _)| *index != verdict.0)
            {
                return Err(unattributable());
            }
            agreed = Some(verdict);
        }
        // `fresh` is non-empty, so the loop above either erred out or settled on one candidate.
        let Some((index, offending)) = agreed else {
            continue;
        };
        rejections.push(PeerRejection {
            index,
            violation: id.requirement(),
            offending,
        });
    }
    Ok(rejections)
}

/// The peer contracts a lock *provably* violates — between two importer-declared packages the
/// dependent demonstrably sees.
/// A violation is counted only on proof:
///
/// - the dependent must be a proven-direct instance ([`direct_dependent_members`], the same rule
///   the pre-apply gate applies) — a transitive optional-peer plugin deep in the graph must not
///   veto a move its own resolver accepted;
/// - the violated peer package must be importer-declared too — a *transitive* peer
///   (`@typescript-eslint/parser`, materialized only as the plugin's auto-installed peer) is the
///   resolver's to place and re-place, so its lag must not veto a direct move the resolver
///   accepted;
/// - each binding is contextual, judged as the layout dictates: on npm's physical tree the peer
///   version is whatever the dependent's own instance resolves by nearest-ancestor lookup
///   ([`InstallPaths::resolve_from`] — hoisting lets disjoint members' packages meet at the root
///   `node_modules`, a nested copy shadows the hoisted one, and a nested *satisfying* copy is
///   equally decisive in the dependent's favor); on pnpm it is the version the importer itself
///   declares ([`MemberIndex::resolved_version`] — peers resolve against the importing context,
///   and each importer's declaration is that context's copy, so two importers may bind two
///   different versions and each is judged on its own);
/// - a context that does not demonstrably bind the peer contributes nothing: absent on the npm
///   lookup path (a possibly-optional peer), or not declared by the pnpm importer (reaching the
///   dependent only transitively — the resolver's business);
/// - and a binding counts only when the bound version is [`RangeMatch::Excludes`]-proven outside
///   the recorded range.
///
/// `workspace` carries the contracts the lock is not authoritative for — every workspace-local
/// package's own `peerDependencies`, including the npm root project's, read from the pre-apply
/// manifests ([`workspace_peer_requirements`]).
/// They are judged by the same proof rules, bound
/// through each contract's [`origin`](WorkspacePeer::origin) directory (npm) or its importer
/// contexts (pnpm), and a local package never moves in a plan — so a resolver move that breaks one
/// is the moved package's fault, and an unplanned collateral move that breaks one is escalated to
/// candidate isolation.
///
/// A lock with no recorded peer contracts (yarn, bun) and no workspace contracts returns empty
/// without any further parsing.
pub(crate) fn proven_peer_violations<L: NodeLock>(
    content: &str,
    workspace: &[WorkspacePeer],
) -> PeerViolations {
    let requirements = L::peer_requirements(content);
    if requirements.is_empty() && workspace.is_empty() {
        return PeerViolations::new();
    }
    let members = L::member_sources(content);
    let install = L::install_paths(content);
    let mut resolved: HashMap<String, HashSet<String>> = HashMap::new();
    for NameVersion { name, version } in L::parse(content).unwrap_or_default() {
        resolved.entry(name).or_default().insert(version);
    }
    let resolved_splits: HashSet<String> = resolved
        .iter()
        .filter(|(_, versions)| versions.len() > 1)
        .map(|(name, _)| name.clone())
        .collect();
    let mut out = PeerViolations::new();
    for pr in requirements {
        let Some(dependent_members) = direct_dependent_members(
            &members,
            install.as_ref(),
            &pr.dependent,
            &pr.dependent_version,
            &resolved_splits,
        ) else {
            continue;
        };
        if !members.declares(&pr.package) {
            continue;
        }
        let mut bindings: Vec<(String, String)> = Vec::new();
        for member in dependent_members {
            let bound = match &install {
                Some(install) => install
                    .member_resolution(&member, &pr.dependent)
                    .and_then(|dependent_instance| {
                        install.resolve_from(dependent_instance.directory, &pr.package)
                    })
                    .map(|instance| instance.version),
                None => members.resolved_version(&member, &pr.package),
            };
            let Some(peer_version) = bound else {
                continue;
            };
            if version::range_match(&pr.range, peer_version) == RangeMatch::Excludes {
                bindings.push((member, peer_version.to_string()));
            }
        }
        if bindings.is_empty() {
            continue;
        }
        out.insert(
            PeerViolationId {
                dependent: pr.dependent,
                dependent_version: pr.dependent_version,
                package: pr.package,
                range: pr.range,
            },
            ViolationDetail { bindings },
        );
    }
    for workspace_peer in workspace {
        let pr = &workspace_peer.requirement;
        // A local package's declared peer must still be a package the workspace itself declares:
        // a contract naming something only the resolver placed transitively is its to re-place.
        if !members.declares(&pr.package) {
            continue;
        }
        let mut bindings: Vec<(String, String)> = Vec::new();
        match &install {
            // The contract binds where the manifest lives — its origin directory, resolved
            // physically.
            // One context, exactly identified: no same-named package elsewhere in the
            // tree can stand in for it.
            Some(install) => {
                if let Some(instance) =
                    install.member_resolution(&workspace_peer.origin, &pr.package)
                    && version::range_match(&pr.range, instance.version) == RangeMatch::Excludes
                {
                    bindings.push((workspace_peer.origin.clone(), instance.version.to_string()));
                }
            }
            // A declaration-isolated layout (pnpm): the contract binds in each importer that has
            // the local package present, against that importer's own declared copy.
            None => {
                for context in &workspace_peer.contexts {
                    let Some(peer_version) = members.resolved_version(context, &pr.package) else {
                        continue;
                    };
                    if version::range_match(&pr.range, peer_version) == RangeMatch::Excludes {
                        bindings.push((context.clone(), peer_version.to_string()));
                    }
                }
            }
        }
        if bindings.is_empty() {
            continue;
        }
        out.insert(
            PeerViolationId {
                dependent: pr.dependent.clone(),
                dependent_version: pr.dependent_version.clone(),
                package: pr.package.clone(),
                range: pr.range.clone(),
            },
            ViolationDetail { bindings },
        );
    }
    out
}

/// The first (smallest-keyed, hence deterministic) peer contract the `after` lock provably
/// violates that the `before` lock did not already cover ([`baseline_covers`]) — the
/// [`settle_landed_candidate`] diff in one call, kept as a test-side convenience for exercising
/// the shared proof and coverage rules against raw lock bodies.
#[cfg(test)]
pub(crate) fn first_new_peer_violation<L: NodeLock>(
    before: Option<&str>,
    after: &str,
) -> Option<crate::lock::PeerRequirement> {
    let baseline = before
        .map(|lock| proven_peer_violations::<L>(lock, &[]))
        .unwrap_or_default();
    proven_peer_violations::<L>(after, &[])
        .into_iter()
        .find(|(id, detail)| !baseline_covers(&baseline, id, detail))
        .map(|(id, _)| id.requirement())
}

/// Names the lock resolves at more than one distinct version — the whole resolved graph, nested
/// copies included, unlike [`multi_version_names`], which counts importer *declarations* only.
/// Tells the gate when npm's name-only importer attribution is ambiguous — consulted only when no
/// physical layout can resolve the split instance-exactly (see [`direct_dependent_members`]).
/// An
/// unparsable lock yields the empty set; unreachable in practice, since the peer requirements
/// that trigger the query parse from the same document.
fn resolved_multi_version_names<L: NodeLock>(content: &str) -> HashSet<String> {
    let mut versions: HashMap<String, HashSet<String>> = HashMap::new();
    for NameVersion { name, version } in L::parse(content).unwrap_or_default() {
        versions.entry(name).or_default().insert(version);
    }
    versions
        .into_iter()
        .filter_map(|(name, set)| (set.len() > 1).then_some(name))
        .collect()
}

/// A peer requirement sourced from a workspace-local package's own `package.json` — the contracts
/// the lock's importer records are NOT authoritative for: a symlinked (`link:`) package's peers
/// live only in its manifest, an injected (`file:`) package's importer entry carries none either,
/// and npm's package-lock deliberately leaves the root project's own peers to this source.
///
/// `origin` is the manifest's own workspace path — the identity that makes this contract's binding
/// context exact rather than name-keyed.
/// It is where the package physically lives, and therefore
/// where its `require`/peer lookups start: node resolves a symlinked workspace package from its
/// real path (`--preserve-symlinks` is off by default), so a consumer's hoisted alias directory is
/// NOT a second binding context — the package always sees its own nested copies first.
///
/// `contexts` are the importer paths the contract binds in on a declaration-isolated layout
/// (pnpm): the package's own directory plus every importer that consumes or declares it.
pub(crate) struct WorkspacePeer {
    pub(crate) requirement: crate::lock::PeerRequirement,
    pub(crate) origin: String,
    pub(crate) contexts: Vec<String>,
}

/// Reads every workspace member manifest and returns its declared peer requirements with the
/// contexts they bind in (see [`WorkspacePeer`]).
/// A member manifest that is missing, unreadable,
/// or nameless is skipped — fail open, the shared rule for every evidence source of this gate.
/// A
/// manifest without a version reports `workspace`, so the blame line still reads naturally.
fn workspace_peer_requirements<L: NodeLock>(
    root: &Utf8Path,
    lock: &str,
    members: &crate::lock::MemberIndex,
) -> Vec<WorkspacePeer> {
    let locals = L::local_package_consumers(lock);
    // Candidate member dirs: every member directory the lock records ([`NodeLock::member_paths`] —
    // including one that declares nothing, whose `package.json` still owns binding peer contracts,
    // and npm's root, whose peers the lock deliberately omits), every importer that declares
    // something, PLUS every local-package target (symlinked `link:` or injected `file:`) — a
    // pure-peer local package can appear in the lock only as the target of its consumers' entries.
    let mut paths: Vec<String> = L::member_paths(lock)
        .into_iter()
        .chain(members.all_paths())
        .chain(locals.keys().cloned())
        .collect();
    paths.sort();
    paths.dedup();
    let mut out = Vec::new();
    for path in paths {
        // Both path sources validate at their parse boundary, but this is the one place lock
        // data reaches the filesystem — keep the workspace-containment check where it matters.
        if !crate::lock::is_workspace_relative(&path) {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(root.join(&path).join("package.json")) else {
            continue;
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        let Some(name) = doc.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        let version = doc
            .get("version")
            .and_then(|value| value.as_str())
            .unwrap_or("workspace");
        let Some(peers) = doc
            .get("peerDependencies")
            .and_then(|value| value.as_object())
        else {
            continue;
        };
        let mut contexts = vec![path.clone()];
        contexts.extend(locals.get(&path).into_iter().flatten().cloned());
        // npm attributes importer declarations by name, so a member consumed as a workspace
        // dependency contributes its declarers as binding contexts too.
        contexts.extend(members.members_for(name, version));
        contexts.sort();
        contexts.dedup();
        for (peer, range) in peers {
            if let Some(range) = range.as_str() {
                out.push(WorkspacePeer {
                    requirement: crate::lock::PeerRequirement {
                        dependent: name.to_string(),
                        dependent_version: version.to_string(),
                        package: peer.clone(),
                        range: range.to_string(),
                    },
                    origin: path.clone(),
                    contexts: contexts.clone(),
                });
            }
        }
    }
    out
}

/// The workspace-manifest peer requirement that structurally excludes `change`'s target, when one
/// exists — the manifest-sourced sibling of [`peer_hold`], judged with the same tri-state range
/// proof.
/// A workspace-local dependent never moves in a plan (it is consumed locally — symlinked or
/// injected — not resolved from the registry), so no co-move exemption applies: only a deliberate
/// edit of its own peer range lifts the hold.
///
/// Unlike [`peer_hold`] this does NOT require the move to cross a compatibility line.
/// A published
/// peer contract is the one range cooldown will not rewrite for the author
/// ([`WIDENABLE_FIELDS`](manifest) omits `peerDependencies`), so the gate must catch *every*
/// provable break or a same-line move would land against a contract that still excludes it: a
/// narrow author-written bound (`>=5.6.0 <5.6.2`) is violated by a mere patch bump, which no
/// compatibility-line test sees.
/// The range proof is unchanged and remains the real test — it must
/// match the current version and provably exclude the target — so widening the trigger can only
/// add holds for demonstrable violations.
pub(crate) fn workspace_peer_hold<'a>(
    change: &Change,
    workspace: &'a [WorkspacePeer],
    multi_version: &HashSet<String>,
    install: Option<&crate::lock::InstallPaths>,
) -> Option<&'a crate::lock::PeerRequirement> {
    if change.downgrade || multi_version.contains(&change.package.name) {
        return None;
    }
    let rewritten = install.map(|install| rewritten_dirs(install, change));
    workspace
        .iter()
        .filter(|workspace_peer| {
            let pr = &workspace_peer.requirement;
            pr.package == change.package.name
                && pr.dependent != change.package.name
                && version::range_match(&pr.range, change.from.as_str()) == RangeMatch::Matches
                && version::range_match(&pr.range, change.to.as_str()) == RangeMatch::Excludes
                // Visibility follows the layout (see `dependent_holds_context`): npm's hoisted
                // tree is judged physically from the manifest's OWN directory — the origin, not
                // any same-named package elsewhere in the tree — and the copy it binds must be a
                // copy this change actually rewrites, not merely a same-version instance
                // elsewhere. pnpm's isolated importers go by context overlap.
                && match (install, &rewritten) {
                    (Some(install), Some(rewritten)) => install
                        .member_resolution(&workspace_peer.origin, &change.package.name)
                        .is_some_and(|instance| rewritten.contains(&instance.directory)),
                    _ => members_overlap(&change.members, &workspace_peer.contexts),
                }
        })
        .map(|workspace_peer| &workspace_peer.requirement)
        .min_by_key(|pr| (&pr.dependent, &pr.dependent_version))
}

/// The install directories `change` actually rewrites: each declaring member's own resolution of
/// the package, when it currently sits at the change's `from` version — moving `host` for
/// `apps/b` rewrites the copy `apps/b` resolves, never an unrelated same-version copy nested in
/// another subtree, so a dependent bound to the latter must not hold the former.
/// A change without
/// member attribution moves the name in every context (the single-context default), so every
/// instance at the current version counts.
fn rewritten_dirs<'i>(install: &'i crate::lock::InstallPaths, change: &Change) -> Vec<&'i str> {
    if change.members.is_empty() {
        return install.instance_dirs(&change.package.name, change.from.as_str());
    }
    let mut dirs: Vec<&str> = change
        .members
        .iter()
        .filter_map(|member| install.member_resolution(&member.path, &change.package.name))
        .filter(|instance| instance.version == change.from.as_str())
        .map(|instance| instance.directory)
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs
}

/// Whether `from → to` crosses an npm compatibility line: [`major_key`](version::major_key)
/// inequality — the same predicate that gates `--major`, so `0.1 → 0.2` crosses (npm caret
/// semantics: the 0.x minor is the breaking axis) — refined for `0.0.x`, where the caret admits
/// nothing beyond the exact version (`^0.0.3` ⇔ `=0.0.3`), so *every* `0.0.x` step is a breaking
/// move even though [`major_key`](version::major_key) keeps the whole range on one `0.0` line and
/// `--major` does not gate inside it.
/// The refinement only widens where the gate still demands proof (a range matching the current
/// version and provably excluding the target), so it can add
/// holds only for true violations.
/// An unparsable version never crosses (fail open).
fn crosses_compatibility_line(from: &str, to: &str) -> bool {
    let (Some(from_version), Some(to_version)) = (version::parse(from), version::parse(to)) else {
        return false;
    };
    if from_version.major == 0 && from_version.minor == 0 && from_version != to_version {
        return true;
    }
    version::major_key(from) != version::major_key(to)
}

/// The lock-recorded peer requirement that structurally excludes `change`'s target, when one exists
/// (see [`partition_peer_held`] for the gating rules).
/// Blame is deterministic: the first blocker by
/// `(dependent, dependent_version)`.
fn peer_hold<'a>(
    change: &Change,
    requirements: &'a [crate::lock::PeerRequirement],
    members: &crate::lock::MemberIndex,
    multi_version: &HashSet<String>,
    resolved_splits: &HashSet<String>,
    moving: &[&Change],
    install: Option<&crate::lock::InstallPaths>,
) -> Option<&'a crate::lock::PeerRequirement> {
    if change.downgrade
        || !crosses_compatibility_line(change.from.as_str(), change.to.as_str())
        || multi_version.contains(&change.package.name)
    {
        return None;
    }
    requirements
        .iter()
        .filter(|pr| {
            pr.package == change.package.name
                && pr.dependent != change.package.name
                && version::range_match(&pr.range, change.from.as_str()) == RangeMatch::Matches
                && version::range_match(&pr.range, change.to.as_str()) == RangeMatch::Excludes
                && dependent_holds_context(change, pr, members, resolved_splits, moving, install)
        })
        .min_by_key(|pr| (&pr.dependent, &pr.dependent_version))
}

/// Whether the peer-declaring dependent is a held-in-place *direct* dependent of the change's
/// importing context — the only shape whose lock-recorded peer range is authoritative for this move
/// (see [`partition_peer_held`] for the full rules and their rationale).
fn dependent_holds_context(
    change: &Change,
    pr: &crate::lock::PeerRequirement,
    members: &crate::lock::MemberIndex,
    resolved_splits: &HashSet<String>,
    moving: &[&Change],
    install: Option<&crate::lock::InstallPaths>,
) -> bool {
    let Some(dependent_members) = direct_dependent_members(
        members,
        install,
        &pr.dependent,
        &pr.dependent_version,
        resolved_splits,
    ) else {
        return false;
    };
    // The dependent itself moves in (one of) its own importing contexts: its target may lift the
    // peer range — joint feasibility is the resolver's decision.
    if moving.iter().any(|other| {
        other.package.name == pr.dependent && members_overlap(&other.members, &dependent_members)
    }) {
        return false;
    }
    // Peers resolve against the importing context — which the layout defines. npm's hoisted tree
    // is judged physically, per direct instance: some member's own dependent copy must resolve the
    // package at a copy this change actually rewrites — directory identity, not merely the same
    // version, since an unrelated same-version instance in another subtree survives the move
    // untouched (disjoint members' packages meet at the root `node_modules`, while a nested
    // copy — conflicting or satisfying — shadows it). pnpm's importers are isolated by
    // declaration, so there disjoint importers keep their own in-range copy and member overlap
    // decides.
    match install {
        Some(install) => {
            let rewritten = rewritten_dirs(install, change);
            dependent_members.iter().any(|member| {
                install
                    .member_resolution(member, &pr.dependent)
                    .and_then(|dependent_instance| {
                        install.resolve_from(dependent_instance.directory, &change.package.name)
                    })
                    .is_some_and(|instance| rewritten.contains(&instance.directory))
            })
        }
        None => {
            change.members.is_empty()
                || change
                    .members
                    .iter()
                    .any(|member| dependent_members.contains(&member.path))
        }
    }
}

/// The importers declaring `dependent` — but only when the lock's attribution *proves* this is
/// the direct instance, `None` otherwise.
/// Shared by the pre-apply gate
/// ([`dependent_holds_context`]) and the post-apply verification ([`proven_peer_violations`]), so
/// the two cannot drift on what "direct" means:
///
/// - no importer attribution → transitive: the resolver may float it, so its recorded peer range
///   is never authoritative;
/// - with a physical layout (npm), attribution is instance-exact even though the declarations are
///   name-only: a declaring member is direct for THIS record exactly when its own
///   nearest-ancestor lookup resolves the dependent at this record's version — a nested copy of
///   the same name (even at another version) neither masquerades as direct nor blinds the real
///   direct copy;
/// - without one (pnpm is version-exact and unaffected; an npm v1 lock records no layout),
///   name-only attribution of a name resolved at several versions cannot single out the direct
///   instance from a nested transitive copy — fail open.
fn direct_dependent_members(
    members: &crate::lock::MemberIndex,
    install: Option<&crate::lock::InstallPaths>,
    dependent: &str,
    dependent_version: &str,
    resolved_splits: &HashSet<String>,
) -> Option<Vec<String>> {
    let dependent_members = members.members_for(dependent, dependent_version);
    if dependent_members.is_empty() {
        return None;
    }
    let Some(install) = install else {
        if !members.version_attributed(dependent, dependent_version)
            && resolved_splits.contains(dependent)
        {
            return None;
        }
        return Some(dependent_members);
    };
    let direct: Vec<String> = dependent_members
        .into_iter()
        .filter(|member| {
            install
                .member_resolution(member, dependent)
                .is_some_and(|instance| instance.version == dependent_version)
        })
        .collect();
    (!direct.is_empty()).then_some(direct)
}

/// Whether a change's member attribution overlaps the dependent's declaring importers.
/// A change
/// with no attribution moves the name in every context (the single-context default).
fn members_overlap(change_members: &[MemberRef], dependent_members: &[String]) -> bool {
    change_members.is_empty()
        || change_members
            .iter()
            .any(|member| dependent_members.contains(&member.path))
}

/// Name the package whose peer/version requirement structurally holds `held` below `target`, scanning
/// the resolved `pnpm-lock.yaml`. pnpm appends a `(peer@x)` suffix to a package key whenever its
/// presence depends on a peer being resolved a certain way, so a held candidate that has *no* matured
/// key in the resolved graph is mutually exclusive with whatever peer the resolver did pick.
/// The named blocker is the unique *other* package that carries a peer-suffixed key — the sibling
/// whose peer
/// choice excluded `held`.
/// When blame is ambiguous (no peer-suffixed sibling, or several) it returns
/// `None`, so the caller falls back to the generic "the resolver rejected this change" message — the
/// same best-effort contract as uv's `unique_edge_requirer`.
pub(crate) fn peer_conflict_blocker(lock: &str, held: &str) -> Option<String> {
    let mut blockers: BTreeSet<String> = BTreeSet::new();
    for name in pnpm_peer_suffixed_names(lock) {
        if name != held {
            blockers.insert(name);
        }
    }
    match blockers.len() {
        1 => blockers.into_iter().next(),
        _ => None,
    }
}

/// Every `packages:` or `snapshots:` key in a `pnpm-lock.yaml` that carries a `(…)` peer
/// disambiguation suffix — returned as the package names used to attribute a held peer conflict to
/// the sibling that forced the peer choice. Both sections must be scanned because the suffix moved
/// with the lock format: pnpm ≤8 wrote peer-suffixed `packages:` keys, while lockfileVersion 9
/// (pnpm 9-11) keeps `packages:` suffix-free and records the peer-resolved identities under
/// `snapshots:` — on a modern lock a `packages:`-only scan finds nothing and every held conflict
/// would self-blame.
fn pnpm_peer_suffixed_names(lock: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in lock.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(stripped) = line.strip_prefix("  ") {
            if !in_packages || stripped.starts_with(' ') {
                continue;
            }
            let key = stripped
                .trim()
                .trim_end_matches(':')
                .trim_matches('\'')
                .trim_matches('"');
            let Some(open) = key.find('(') else { continue };
            let base = key[..open].to_string();
            if let Some(NameVersion { name, .. }) = crate::lock::split_name_version(&base) {
                out.push(name);
            }
        } else {
            in_packages = line.starts_with("packages:") || line.starts_with("snapshots:");
        }
    }
    out
}

#[cfg(test)]
#[path = "peers/tests.rs"]
mod tests;
