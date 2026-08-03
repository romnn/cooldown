//! The generic JavaScript/TypeScript [`Tool`]: detection, the resolved graph from a lockfile, npm
//! registry publish times, and driver-backed re-resolution/apply. The lockfile format and driver
//! binary are supplied by a [`NodeLock`] type parameter, so npm, pnpm, yarn, and bun are all the
//! same adapter specialised over their lock format — they share the npm registry and version model
//! and differ only in how their lock is parsed and how their CLI re-pins a dependency.

use crate::apply::landing::{
    CandidateLanding, OwnedStep, absolute_cutoff_from_project, candidate_landing,
    restore_after_owned_step, run_candidate_landing_with, target_in_declared_range,
    window_minutes_from_cutoff,
};
use crate::lock::{NameVersion, NodeLock};
use crate::manifest;
use crate::native::{
    ConfigStringList, set_yaml_block_list, set_yaml_scalar, set_yaml_string_map, window_minutes,
};
use crate::nodecmd::NodeCmd;
use crate::peers::{
    JointResolve, PeerBaseline, PeerEvidence, PeerPartition, PeerViolations, WorkspacePeer,
    journaled_lock, partition_peer_held, peer_conflict_blocker, peer_held_skip,
    plan_peer_rejections, proven_peer_violations, settle_landed_candidate,
};
use crate::registry::{NPM, NpmRegistry};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{
    RegistryVersionClassifier, build_registry_releases, skipped_on_apply_error,
    verify_current_unknown,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, CoreError, DepScope, Dependency,
    FetchContext, LockVerifyReport, MemberRef, NativePolicyLayer, PackageId, PackageRegistry, Plan,
    PreparedMutation, Project, ProjectMarker, ProjectMutationJournal, RawRelease, Release,
    ReleaseFetcher, ReleaseOrder, ReleaseQuality, ResolvedPolicy, Result, RewriteMode, SkipReason,
    Skipped, SyncReport, SyncScope, ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::marker::PhantomData;

struct WholeGraphInputs {
    exact_pins: Vec<(String, String)>,
    importer_filters: Vec<String>,
}

/// The resolved lock's `name -> version` map, the snapshot `apply` diffs before/after the whole-graph
/// re-resolve so *every* net version change is reported (the planned moves, the collateral churn the
/// joint resolve forced on other packages, and the candidates left held below their target). A name
/// that resolves to several versions (a duplicated graph copy) keeps its newest, so a moved direct
/// declaration is never masked by a stale transitive copy of the same name.
fn locked_versions<L: NodeLock>(content: &str) -> HashMap<String, String> {
    let mut versions: HashMap<String, String> = HashMap::new();
    for NameVersion { name, version } in L::parse(content).unwrap_or_default() {
        match versions.entry(name) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if version::compare(&version, slot.get()).is_gt() {
                    *slot.get_mut() = version;
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(version);
            }
        }
    }
    versions
}

/// Resolve each member path to its `package.json` "name", read once per `dependencies()` call. A
/// path with no readable name is omitted, so the caller falls back to showing the path itself.
fn member_names(root: &Utf8Path, paths: &HashSet<String>) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for path in paths {
        let manifest = if path == "." {
            root.join("package.json")
        } else {
            root.join(path).join("package.json")
        };
        let name = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|doc| {
                doc.get("name")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            });
        if let Some(name) = name {
            names.insert(path.clone(), name);
        }
    }
    names
}

/// Renders pnpm's location selector for a lockfile importer ID.
///
/// Importer IDs are pnpm-owned portable strings, not host paths. Keeping the ID opaque avoids
/// platform-dependent parsing; the `./` prefix forces location selection. Direct process arguments
/// need no shell escaping.
fn pnpm_location_filter(path: &str) -> String {
    if path == "." {
        ".".to_string()
    } else {
        format!("./{path}")
    }
}

/// The JavaScript/TypeScript implementation of the [`Tool`] port, generic over a [`NodeLock`].
///
/// It detects projects by their lockfile, reads the resolved graph from that lock, recovers
/// direct/transitive classification from lock importer data or the root `package.json`, and resolves
/// publish times from the shared [`NpmRegistry`]. npm has no native cooldown config, so
/// [`native_policy`] is always empty.
///
/// [`native_policy`]: ToolRead::native_policy
pub struct NpmTool<L> {
    registry: NpmRegistry,
    cmd: NodeCmd,
    _lock: PhantomData<fn() -> L>,
}

impl<L: NodeLock> NpmTool<L> {
    /// Creates the adapter from a configured [`NpmRegistry`].
    #[must_use]
    pub fn new(registry: NpmRegistry) -> Self {
        NpmTool {
            registry,
            cmd: NodeCmd::new(L::BIN),
            _lock: PhantomData,
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`NpmRegistry`].
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        NpmTool::new(NpmRegistry::new(http))
    }

    /// Enables package-document revalidation for version-adopting runs, so the mutable `latest`
    /// dist-tag ceiling is judged against the registry's current state rather than a cached copy
    /// up to a listing-TTL stale (see [`NpmRegistry::with_listing_revalidation`]).
    #[must_use]
    pub fn with_listing_revalidation(mut self, revalidate: bool) -> Self {
        self.registry = self.registry.with_listing_revalidation(revalidate);
        self
    }
}

pub(crate) fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// Builds the sorted, deduplicated [`Release`] list the core consumes from the registry's raw
/// releases. npm and JSR both serve one artifact per version with no per-artifact split, so (unlike
/// PyPI) there is no artifact-scope handling here.
///
/// `latest_tag` is the version the registry's `latest` dist-tag names, when known: every release
/// ordered above it is marked [`beyond_latest_tag`](Release::beyond_latest_tag) so the core can cap
/// adoption at the maintainer's own "this is current" pointer. JSR and Deno pass `None` (no
/// dist-tags there), leaving every release unmarked.
pub(crate) fn build_releases(
    current: &str,
    raw: Vec<RawRelease>,
    latest_tag: Option<&str>,
) -> Vec<Release> {
    let mut releases = build_registry_releases(
        current,
        raw,
        RegistryVersionClassifier {
            is_valid: |value| version::parse(value).is_some(),
            compare: version::compare,
            major_key: version::major_key,
            major_number: version::major_number,
            classify_kind: version::classify_kind,
            classify_quality,
        },
    );
    if let Some(tag) = latest_tag {
        mark_beyond_latest_tag(&mut releases, tag);
    }
    releases
}

/// Marks every release ordered above the `latest`-tagged version as
/// [`beyond_latest_tag`](Release::beyond_latest_tag).
///
/// Fails open when the tag names a version absent from the sorted release list (a registry
/// inconsistency): nothing is marked, so no ceiling applies — the conservative direction for a
/// signal that only ever *restricts* adoption.
fn mark_beyond_latest_tag(releases: &mut [Release], tag: &str) {
    let Some(tag_order) = releases
        .iter()
        .find(|release| version::compare(release.version.as_str(), tag).is_eq())
        .map(|release| release.order.clone())
    else {
        return;
    };
    for release in releases {
        release.beyond_latest_tag = release.order > tag_order;
    }
}

/// Captures the lockfile and every package manifest this plan could rewrite.
fn journal<L: NodeLock>(project: &Project, plan: &Plan) -> Result<ProjectMutationJournal> {
    let mut seen = BTreeSet::new();
    let mut rels = Vec::new();
    push_journal_rel(&mut rels, &mut seen, Utf8PathBuf::from(L::LOCKFILE));
    if let Some(native) = L::NATIVE_MIN_AGE_FILE {
        push_journal_rel(&mut rels, &mut seen, Utf8PathBuf::from(native));
    }
    for change in &plan.changes {
        for rel in manifest::manifest_rels(&change.members) {
            push_journal_rel(&mut rels, &mut seen, rel);
        }
    }
    ProjectMutationJournal::capture(&project.root, rels)
}

fn push_journal_rel(
    rels: &mut Vec<Utf8PathBuf>,
    seen: &mut BTreeSet<Utf8PathBuf>,
    rel: Utf8PathBuf,
) {
    if seen.insert(rel.clone()) {
        rels.push(rel);
    }
}

#[async_trait]
impl<L: NodeLock> ToolRead for NpmTool<L> {
    fn id(&self) -> ToolId {
        L::ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            // npm, pnpm, yarn, and bun all resolve from the npm registry, whose mutable `latest`
            // dist-tag caps candidate adoption (Release::beyond_latest_tag).
            has_dist_tags: true,
            can_sync: true,
            artifact_granular: false,
        }
    }

    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        // The lockfile sits at the workspace root; nested `package.json`s share it (no nested lock).
        cooldown_core::ProjectDetection::Primary(ProjectMarker {
            lockfile: L::LOCKFILE,
            manifest: "package.json",
            alternate_manifests: &[],
            workspace_root: true,
        })
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let resolved = L::parse(&content)?;
        // Which workspace member(s) declare each dependency, for source attribution; empty for lock
        // formats without per-member data (yarn classic, bun). Member paths are resolved to package
        // names once, by reading each member's `package.json`.
        let member_index = L::member_sources(&content);
        let member_names = member_names(&project.root, &member_index.all_paths());
        // Direct-ness comes from the same importer data as attribution: a dependency is direct iff an
        // importer declares it. For pnpm this is version-exact, so a name declared at one version but
        // only pulled in transitively at another (a second copy in the graph) is split correctly —
        // the transitive copy is not reported as a direct dependency with a blank source. Lock
        // formats without importer data fall back to the root `package.json`'s declared names.
        let manifest_direct = if member_index.is_authoritative() {
            None
        } else {
            Some(manifest::direct_names(&project.manifest)?)
        };

        let mut seen = HashSet::new();
        let mut deps = Vec::new();
        for NameVersion { name, version } in resolved {
            let member_paths = member_index.members_for(&name, &version);
            let is_direct = match &manifest_direct {
                Some(names) => names.contains(&name),
                None => !member_paths.is_empty(),
            };
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            if !seen.insert((name.clone(), version.clone())) {
                continue; // a name can resolve to the same version via several paths
            }
            let members: Vec<MemberRef> = member_paths
                .into_iter()
                .map(|path| MemberRef {
                    name: member_names
                        .get(&path)
                        .cloned()
                        .unwrap_or_else(|| path.clone()),
                    path,
                })
                .collect();
            let pinned = member_index.is_exact_pinned(&name, &version);
            let declared_bound = if is_direct {
                manifest::declared_bound(&project.root, &members, &name)?
            } else {
                None
            };
            deps.push(Dependency {
                package: PackageId::new(L::ID, name, Some(NPM.to_string())),
                current: Version::new(version.clone()),
                current_quality: classify_quality(&version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
                declared_bound,
                members,
                pinned,
            });
        }
        Ok(deps)
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        // npm has no standard in-manifest cooldown/freeze field, so there is no native layer.
        Ok(None)
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<LockVerifyReport> {
        let Some(args) = L::verify_current_args() else {
            return Ok(verify_current_unknown(L::LOCKFILE));
        };
        self.cmd
            .lock_report(&project.root, &args, &format!("{} is current", L::LOCKFILE))
            .await
    }
}

#[async_trait]
impl<L: NodeLock> ReleaseFetcher for NpmTool<L> {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let packument = self.registry.packument(&dep.package).await?;
        Ok(build_releases(
            dep.current.as_str(),
            packument.releases,
            packument.latest_tag.as_deref(),
        ))
    }

    fn classify_declared_bound(&self, dep: &Dependency, releases: &mut [Release]) {
        if let Some(requirement) = dep.declared_bound.as_deref() {
            for release in releases {
                release.beyond_declared_bound =
                    !version::version_in_range(requirement, release.version.as_str());
            }
        }
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self
            .registry
            .published_at(&dep.package, &dep.current, &[])
            .await?;
        Ok(Release {
            version: dep.current.clone(),
            order: ReleaseOrder(Vec::new()),
            major: version::major_key(dep.current.as_str()),
            major_number: version::major_number(dep.current.as_str()),
            kind_from_current: None,
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

/// Whether the re-locked graph resolves `change` at exactly its target, judged per declaring
/// member when the lock carries member-scoped entries and by the name's newest copy otherwise.
///
/// A successful install command is not proof: `npm install <name>@<version> --before=<cutoff>`
/// exits 0 yet lands the newest pre-cutoff version when the requested one is newer than the
/// cutoff, so the landing must be read back from the lock.
fn exact_target_reached<L: NodeLock>(project: &Project, change: &Change) -> Result<bool> {
    let content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
    let newest = locked_versions::<L>(&content);
    let target = change.to.as_str();
    if change.members.is_empty() {
        return Ok(newest
            .get(&change.package.name)
            .is_some_and(|version| version == target));
    }
    let members = L::member_sources(&content);
    let member_versions: Vec<Option<&str>> = change
        .members
        .iter()
        .map(|member| members.resolved_version(&member.path, &change.package.name))
        .collect();
    if member_versions.iter().any(Option::is_some) {
        return Ok(member_versions
            .into_iter()
            .all(|version| version == Some(target)));
    }
    Ok(newest
        .get(&change.package.name)
        .is_some_and(|version| version == target))
}

/// A net version change `apply` derived from the before/after lock diff for a package the plan did not
/// itself name — collateral movement the whole-graph re-resolve forced. Reported so no package's
/// version change is ever silent: a transitive pushed backward (or forward) to keep the lock
/// consistent surfaces as its own report row.
fn collateral_change<L: NodeLock>(name: &str, from: &str, to: &str) -> Change {
    Change {
        package: PackageId::new(L::ID, name.to_string(), Some(NPM.to_string())),
        from: Version::new(from.to_string()),
        to: Version::new(to.to_string()),
        // A collateral move is transitive consistency churn, not a directly-declared bump; its update
        // kind is informational only and `Minor` is the neutral label the renderer shows.
        kind: cooldown_core::UpdateKind::Minor,
        downgrade: version::compare(to, from).is_lt(),
        direct: false,
        members: Vec::new(),
    }
}

/// The net version changes of the before/after lock diff that `applied` does not already report,
/// as sorted collateral rows.
///
/// Exclusion is by landing spot — an applied row for the same name whose target semantically
/// equals the movement's destination — not by planned package name: a planned candidate the
/// resolve *held* can still have been floated off its baseline, and that real movement must
/// surface beside its held skip row instead of being silently dropped. An applied row claiming a
/// *different* landing (a directional overshoot the executor re-verifies into a skip) does not
/// suppress the movement row either. Matching the destination rather than the exact `(from, to)`
/// pair keeps a candidate planned off a stale duplicate copy — whose newest-copy baseline differs
/// from the planned `from` — from double-reporting its move.
fn collateral_changes<L: NodeLock>(
    before: &HashMap<String, String>,
    after: &HashMap<String, String>,
    applied: &[Change],
) -> Vec<Change> {
    let reported = |name: &str, to: &str| {
        applied.iter().any(|change| {
            change.package.name == name && version::compare(change.to.as_str(), to).is_eq()
        })
    };
    let mut changes: Vec<Change> = before
        .iter()
        .filter_map(|(name, from)| {
            let to = after.get(name)?;
            (version::compare(from, to).is_ne() && !reported(name, to))
                .then(|| collateral_change::<L>(name, from, to))
        })
        .collect();
    changes.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    changes
}

/// Whether a planned candidate landed at or beyond its target, respecting the move's direction (a
/// forward move must reach at/above its target, a downgrade at/below it).
///
/// Checked **per declaring member**, not against the name's newest copy: a multi-version dependency can
/// leave one member short of the target even though the name's newest copy — a higher line owned by
/// another member — already sits at it. Checking only the newest copy would falsely report such a
/// candidate as landed.
///
/// A candidate landed when *at least one* of its declaring members reached the target. It is held only
/// when *no* declaring member reached it, which is exactly the cross-line / peer-only hold `outdated`
/// must not call adoptable. Falls back to the newest copy when the change carries no member attribution
/// (a collateral move) or the lock has no per-member version data.
fn reached(
    after_newest: &HashMap<String, String>,
    after_members: &crate::lock::MemberIndex,
    change: &Change,
) -> bool {
    let name = change.package.name.as_str();
    let satisfied = |landed: &str| {
        let ordering = version::compare(landed, change.to.as_str());
        if change.downgrade {
            ordering.is_le()
        } else {
            ordering.is_ge()
        }
    };
    if change.members.is_empty() {
        return after_newest
            .get(name)
            .map(String::as_str)
            .is_some_and(satisfied);
    }
    change.members.iter().any(|member| {
        after_members
            .resolved_version(&member.path, name)
            .is_some_and(satisfied)
    })
}

impl<L: NodeLock> NpmTool<L> {
    async fn run_candidate_landing(
        &self,
        project: &Project,
        candidate_journal: &ProjectMutationJournal,
        landing: &CandidateLanding,
    ) -> Result<OwnedStep> {
        run_candidate_landing_with(&self.cmd, project, candidate_journal, landing).await
    }

    /// For each change, moves the lock with a lock-only update or an exact pin around cooldown's
    /// authorized manifest edits, then reports collateral lock movements.
    ///
    /// npm's `--before` constrains the complete resolved tree, so even a per-package command can move
    /// transitives.
    /// Diffing the journaled lock against the final lock keeps those movements visible.
    async fn apply_per_package(
        &self,
        project: &Project,
        plan: &Plan,
        baseline_journal: &ProjectMutationJournal,
        workspace: &[WorkspacePeer],
    ) -> Result<ApplyReport> {
        let before = journaled_lock::<L>(baseline_journal)
            .map(locked_versions::<L>)
            .unwrap_or_default();
        let mut report = ApplyReport::default();
        let mut violation_baseline: Option<PeerViolations> = None;
        for change in &plan.changes {
            // A failed later candidate must not leak its widened manifest when an earlier sibling
            // succeeded and makes the outer batch committable.
            // Capture the state after those earlier successes so this candidate can be restored
            // independently.
            let candidate_plan = Plan {
                changes: vec![change.clone()],
                ..plan.clone()
            };
            let candidate_journal = journal::<L>(project, &candidate_plan)?;
            let Some(landing) = candidate_landing::<L>(project, change, plan.rewrite)? else {
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::NotEligible,
                    offending: Some(change.package.clone()),
                    detail: None,
                });
                continue;
            };
            let attempt = self
                .run_candidate_landing(project, &candidate_journal, &landing)
                .await?;
            match attempt.result {
                Ok(()) if exact_target_reached::<L>(project, change)? => {
                    settle_landed_candidate::<L>(
                        project,
                        change,
                        &candidate_journal,
                        &attempt.postimage,
                        workspace,
                        &mut violation_baseline,
                        &mut report,
                    )?;
                }
                Ok(()) => {
                    restore_after_owned_step(&candidate_journal, &attempt.postimage)?;
                    report.skipped.push(Skipped {
                        change: change.clone(),
                        reason: SkipReason::ResolverConflict,
                        offending: Some(change.package.clone()),
                        detail: None,
                    });
                }
                Err(error) if error.is_local_environment_failure() => {
                    restore_after_owned_step(&candidate_journal, &attempt.postimage)?;
                    return Err(error);
                }
                Err(error) => {
                    restore_after_owned_step(&candidate_journal, &attempt.postimage)?;
                    report.skipped.push(skipped_on_apply_error(change, error)?);
                }
            }
        }
        let after_content = match std::fs::read_to_string(project.root.join(L::LOCKFILE)) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let after = after_content
            .as_deref()
            .map(locked_versions::<L>)
            .unwrap_or_default();
        let collateral = collateral_changes::<L>(&before, &after, &report.applied);
        report.applied.extend(collateral);
        Ok(report)
    }

    /// Re-resolve the **whole** importer graph jointly (pnpm), pinning every planned candidate to
    /// its EXACT per-package target, then report the full before/after lock diff — the proven
    /// cargo/go pattern ported to pnpm.
    /// Peer verification can reject candidates and re-resolve, so one apply may run several native
    /// resolves (see [`Self::resolve_and_verify_peers`]).
    ///
    /// One importer-filtered `pnpm update <pkg>@<target> … --lockfile-only --no-save` jointly
    /// re-resolves the affected graph, settling mutually-exclusive peer conflicts at a single fixed
    /// point instead of ping-ponging between per-package updates.
    /// Each `<pkg>@<target>` is the candidate's own `change.to`, computed by cooldown-core under that
    /// package's window, so a package with a *stricter* per-package window lands at its older target
    /// rather than
    /// overshooting onto the global-window-newest — the gap a bare `--latest` left, since pnpm's
    /// `minimumReleaseAge` is a single global knob with no per-package publish-date cutoff.
    /// This mirrors cargo's `update --precise <to>` and go's `get module@<to>`: the per-package target
    /// already
    /// encodes the per-package window, so pinning it enforces that window exactly.
    ///
    /// `minimumReleaseAge` is passed as the *transitive* floor.
    /// A persisted native policy can reject an older lock before applying the exact pins that would
    /// repair it.
    /// For that migration state,
    /// pnpm is rerun with temporary exact overrides for the planned targets and `--trust-lockfile`;
    /// this skips only the rejected starting-lock preflight while the age floor still governs the
    /// replacement graph.
    /// The original native config is restored before pnpm settles the lock again.
    ///
    /// The report is the diff of the journal's pre-apply lock against the result, so every planned
    /// candidate is reported reached or held (naming the conflicting peer where attributable) and
    /// every collateral move of an unplanned package surfaces as its own row.
    /// A resolver failure after the repair retry marks the conflicting candidates held.
    /// The accepted result is
    /// post-verified against the pre-apply peer contracts *between workspace-declared packages in
    /// a context that demonstrably binds them* — the lock's own records plus every workspace
    /// member manifest's ([`proven_peer_violations`]): pnpm only warns on a peer mismatch, so a
    /// candidate whose landing provably breaks such a contract (its pair held outside the plan) is
    /// rejected `peer_held` and the remainder re-resolved rather than committing the break.
    async fn apply_whole_graph(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        workspace: &[WorkspacePeer],
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }

        // The pre-apply lock is captured in the journal.
        // Both the newest-version map and the multi-version set are derived from this one copy, so
        // both see exactly the lock the resolve starts from without another disk read.
        let before_content = journaled_lock::<L>(journal);
        let before = before_content.map(locked_versions::<L>).unwrap_or_default();
        let multi_version = before_content
            .map(multi_version_names::<L>)
            .unwrap_or_default();

        // pnpm's `minimumReleaseAge` is a *rolling* age, so the cutoff is realized against the
        // current instant.
        // An absolute `--freeze` cutoff becomes `now - freeze` minutes — equivalent to the
        // freeze date as long as the same `now` governs both the seed and this resolve (it does:
        // wall-clock advances only seconds between them, far below the day-scale window under test).
        // It is passed only as the *transitive* floor here; each planned candidate is pinned to its
        // exact per-package target, so its own window is enforced by the pin rather than this cap.
        let window_minutes =
            window_minutes_from_cutoff(project.exclude_newer.as_deref(), jiff::Timestamp::now());

        let mut peer_skips: Vec<Skipped> = Vec::new();
        let after_content = self
            .resolve_and_verify_peers(
                project,
                &JointResolve {
                    plan,
                    journal,
                    multi_version: &multi_version,
                    window_minutes,
                    workspace,
                },
                &mut peer_skips,
            )
            .await?;
        let after = locked_versions::<L>(&after_content);
        // Per-importer resolved versions, so a candidate's landing is judged at *its* member rather
        // than the name's newest copy — the multi-version float leaves a lower line short of a
        // cross-line target the higher line already satisfies.
        let after_members = L::member_sources(&after_content);

        // A peer-rejected candidate already carries its structured skip row; the diff loop below
        // must not add a second (resolver-conflict) verdict for it.
        let peer_rejected: HashSet<(&str, &str)> = peer_skips
            .iter()
            .map(|skip| (skip.change.package.name.as_str(), skip.change.to.as_str()))
            .collect();
        for change in &plan.changes {
            let name = change.package.name.as_str();
            if peer_rejected.contains(&(name, change.to.as_str())) {
                continue;
            }
            // Whether the lock's version for this name actually moved.
            // A name can resolve to several copies in a pnpm graph; `before`/`after` track its
            // *newest* copy, so a candidate planned
            // off a stale duplicate copy whose newest copy is already at the target shows no net move.
            // Reporting only genuine moves keeps the report set equal to the lock-diff set: a converged
            // re-run, where nothing moved, reports zero applied (no oscillation).
            let moved = match (before.get(name), after.get(name)) {
                (Some(from), Some(to)) => version::compare(from, to).is_ne(),
                (None, Some(_)) | (Some(_), None) => true,
                (None, None) => false,
            };
            if reached(&after, &after_members, change) {
                if moved {
                    report.applied.push(change.clone());
                }
                // Reached its target without a net lock move because a duplicate copy of the same
                // name is at the target.
                // This is a no-op, neither applied nor held.
            } else if multi_version.contains(name) {
                // A dependency declared at multiple versions across the workspace is deliberately
                // kept in range instead of pinned to the target.
                // That is a conservative hold, not a resolver conflict, and it must not be advertised
                // as adoptable: `outdated`'s verify reclassifies it blocked.
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::MultiVersionHeld,
                    offending: None,
                    detail: None,
                });
            } else {
                // The joint resolve could not place this candidate at its target without breaking
                // the lock because a mutually-exclusive peer won.
                // Name the sibling whose peer choice excluded it so the report says
                // "held: conflicts with <pkg>"; absent a unique blocker it falls back to the
                // candidate itself.
                let offender =
                    peer_conflict_blocker(&after_content, name).unwrap_or_else(|| name.to_string());
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::ResolverConflict,
                    offending: Some(PackageId::new(L::ID, offender, Some(NPM.to_string()))),
                    detail: None,
                });
            }
        }

        report.skipped.extend(peer_skips);

        // The hard requirement is that no net version change to any package may be omitted.
        // Every moved package the applied rows above do not already report is surfaced as its own collateral
        // applied row — including a *held* candidate the resolve still floated off its baseline
        // (whose skip row alone would hide that real move).
        let collateral = collateral_changes::<L>(&before, &after, &report.applied);
        report.applied.extend(collateral);
        Ok(report)
    }

    /// Runs the joint resolve to a verified fixed point and returns the accepted lock content.
    /// pnpm only *warns* on a peer mismatch, so the resolve can commit a graph that provably
    /// breaks a recorded contract between two importer-declared packages when one side of a pair
    /// is missing from the plan (a host held by a ceiling while its dependent moves —
    /// `react-dom@19(react@18)` requiring `react@^19`).
    ///
    /// Each round runs the resolve, then diffs the proven violations against the pre-apply
    /// baseline (gathered once — see [`PeerBaseline`]) and rejects every candidate a violation
    /// uniquely proves culpable ([`plan_peer_rejections`]) with structured `peer_held` blame; the
    /// journal is restored and the remainder re-resolved, so unrelated moves survive without the
    /// caller's bisect and extra rounds correspond only to real cascades.
    /// An unattributable violation propagates as a non-local rejection for candidate isolation.
    /// The rounds are
    /// bounded by the plan length (each continuing round removes at least one candidate); when
    /// every candidate is rejected, the restored pre-apply lock is the result.
    async fn resolve_and_verify_peers(
        &self,
        project: &Project,
        inputs: &JointResolve<'_>,
        peer_skips: &mut Vec<Skipped>,
    ) -> Result<String> {
        let &JointResolve {
            plan,
            journal,
            multi_version,
            window_minutes,
            workspace,
        } = inputs;
        let before_content = journaled_lock::<L>(journal);
        let baseline = PeerBaseline::gather::<L>(before_content, workspace);
        let mut active = plan.clone();
        loop {
            let resolve_result = self
                .whole_graph_resolve(project, &active, multi_version, window_minutes)
                .await;
            let mut resolve = OwnedStep::capture(resolve_result, journal)?;
            match resolve.result {
                Ok(()) => {}
                Err(error) if error.is_local_environment_failure() => {
                    restore_after_owned_step(journal, &resolve.postimage)?;
                    return Err(error);
                }
                Err(error)
                    if minimum_age_lock_rejected(&error)
                        && window_minutes.is_some_and(|minutes| minutes > 0)
                        && L::NATIVE_MIN_AGE_FILE.is_some() =>
                {
                    // A persisted minimumReleaseAge validates the starting lock before pnpm applies
                    // the exact pins.
                    // Restore any partial resolver work, then rebuild
                    // through temporary exact overrides while retaining the age floor.
                    restore_after_owned_step(journal, &resolve.postimage)?;
                    self.repair_policy_rejected_graph(
                        project,
                        &active,
                        multi_version,
                        window_minutes,
                    )
                    .await?;
                    resolve.postimage = journal.capture_state()?;
                }
                // The joint resolve is unsatisfiable as a whole.
                // Propagate the failure so the
                // caller's `apply_resilient` can isolate the offending candidate(s) (an
                // unfetchable version, one side of a conflict) and apply the rest, instead of
                // holding every candidate.
                // The caller restores the journal, so no partial lock is kept.
                Err(error) => {
                    restore_after_owned_step(journal, &resolve.postimage)?;
                    return Err(error);
                }
            }

            let after_content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
            if let Some(detail) = new_lock_inconsistency::<L>(before_content, &after_content) {
                return Err(CoreError::StaleLock(detail));
            }
            let current = proven_peer_violations::<L>(&after_content, workspace);
            let mut rejections = plan_peer_rejections(&baseline, &current, &active, multi_version)?;
            if rejections.is_empty() {
                return Ok(after_content);
            }
            // Highest index first, so each removal leaves the remaining indices valid.
            rejections.sort_by_key(|rejection| std::cmp::Reverse(rejection.index));
            for rejection in rejections {
                let change = active.changes.remove(rejection.index);
                peer_skips.push(peer_held_skip::<L>(
                    &change,
                    &rejection.violation,
                    rejection.offending,
                ));
            }
            restore_after_owned_step(journal, &resolve.postimage)?;
            if active.changes.is_empty() {
                // Every candidate was rejected; the restored journal is the result.
                return Ok(before_content.unwrap_or_default().to_string());
            }
        }
    }

    /// Build the per-candidate pins, widen the manifests the exact pins need, then run one joint
    /// resolve filtered to the declaring importers.
    ///
    /// A candidate held at a single version across the workspace is **exact-pinned** to its
    /// per-package target (`name@target`): the resolve lands it at exactly that version, honoring a
    /// stricter-than-global per-package window with no overshoot.
    /// A candidate a member declares at a
    /// version *other* members also hold at a different version (a v4/v5 split, which pnpm keeps like
    /// cargo) is skipped instead: exact-pinning one target would collapse every other copy onto it, and
    /// pnpm's bare `update <name>` can write an out-of-range lock entry while `--no-save` leaves the
    /// manifest unchanged.
    /// The pre-apply lock identifies those multi-version names; a missing or unparsable lock means
    /// nothing is multi-version yet, so every pin is exact.
    ///
    /// Widen is for the exact pins only, and only when their target is out of the declared range
    /// (`Auto`) or always (`Always`).
    /// It is mandatory there: `pnpm update <pkg>@<target> --no-save`
    /// re-pins the lock to an out-of-range target but leaves the manifest as written, so the next
    /// resolve (which re-resolves any package it is not pinning, against its manifest range) snaps the
    /// candidate back into range and breaks the fixed point.
    /// A multi-version candidate is never widened because widening would let it cross its own range
    /// boundary, the very line we are preserving.
    ///
    /// Each declaring member becomes a pnpm portable location filter.
    /// This reaches root and member importers without relying on package names or running the update
    /// in unrelated workspace
    /// packages, where an unmatched package selector can otherwise move unrelated direct
    /// dependencies.
    async fn whole_graph_resolve(
        &self,
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
        window_minutes: Option<i64>,
    ) -> Result<()> {
        let inputs = Self::prepare_whole_graph_inputs(project, plan, multi_version)?;
        if inputs.exact_pins.is_empty() {
            return Ok(());
        }
        self.joint_resolve(
            project,
            &inputs.exact_pins,
            &inputs.importer_filters,
            window_minutes,
        )
        .await?;
        // The up-front pass already widened every out-of-range exact target, so a candidate the resolve
        // still left short of its target is blocked by *another* package's requirement (a peer
        // conflict), which widening its own declared range cannot resolve — the lock diff reports it
        // held.
        // No post-resolve re-widen loop is needed.
        Ok(())
    }

    fn prepare_whole_graph_inputs(
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
    ) -> Result<WholeGraphInputs> {
        let mut pins: Vec<(String, String)> = Vec::with_capacity(plan.changes.len());
        let mut importer_filters = Some(BTreeSet::new());
        for change in &plan.changes {
            let name = change.package.name.clone();
            if multi_version.contains(&name) {
                // Preserve every distinct line.
                // A bare pnpm update can write an out-of-range lock entry while leaving package.json
                // untouched.
                continue;
            }
            // Exact-pin: widen the owning manifest when the target is out of range so the exact lock
            // pin stays consistent with `package.json`.
            // A candidate not declared in any owning manifest (`target_in_declared_range` returns
            // `false`) is widened too, so the pin is never left dangling against a range that
            // excludes it.
            let widen = match plan.rewrite {
                RewriteMode::Always => true,
                RewriteMode::Auto => !target_in_declared_range(project, change)?,
            };
            if widen {
                manifest::widen_constraints(
                    &project.root,
                    &change.members,
                    &change.package.name,
                    change.to.as_str(),
                    plan.rewrite,
                )?;
            }
            if change.members.is_empty() {
                importer_filters = None;
            } else if let Some(filters) = &mut importer_filters {
                filters.extend(
                    change
                        .members
                        .iter()
                        .map(|member| pnpm_location_filter(&member.path)),
                );
            }
            pins.push((name, change.to.as_str().to_string()));
        }
        let filters = match importer_filters {
            Some(filters) => filters.into_iter().collect::<Vec<_>>(),
            None => Vec::new(),
        };
        Ok(WholeGraphInputs {
            exact_pins: pins,
            importer_filters: filters,
        })
    }

    async fn joint_resolve(
        &self,
        project: &Project,
        pins: &[(String, String)],
        filters: &[String],
        window_minutes: Option<i64>,
    ) -> Result<()> {
        let Some(args) = L::whole_graph_args(pins, filters, window_minutes) else {
            return Ok(());
        };
        self.cmd.run(&project.root, &args).await
    }

    async fn repair_policy_rejected_graph(
        &self,
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
        window_minutes: Option<i64>,
    ) -> Result<()> {
        let inputs = Self::prepare_whole_graph_inputs(project, plan, multi_version)?;
        if inputs.exact_pins.is_empty() {
            return Ok(());
        }
        let native = L::NATIVE_MIN_AGE_FILE.ok_or_else(|| {
            CoreError::System("pnpm native config path is unavailable".to_string())
        })?;
        let native_rel = Utf8PathBuf::from(native);
        let native_snapshot = ProjectMutationJournal::capture(&project.root, [&native_rel])?;
        let configured_exclusions = self
            .configured_value::<ConfigStringList>(project, "minimumReleaseAgeExclude")
            .await?
            .into_vec();
        let exclusions = minimum_age_repair_exclusions(plan, configured_exclusions);
        let mut overrides = self
            .configured_value::<BTreeMap<String, String>>(project, "overrides")
            .await?;
        overrides.extend(inputs.exact_pins);

        let temporary_result = async {
            set_yaml_string_map(&project.root.join(&native_rel), "overrides", &overrides)?;
            let args =
                L::policy_repair_args(window_minutes, &exclusions, true).ok_or_else(|| {
                    CoreError::System("pnpm policy repair command is unavailable".to_string())
                })?;
            self.cmd
                .run(&project.root, &args)
                .await
                .map_err(propagate_repeated_minimum_age_rejection)
        }
        .await;
        let native_postimage = native_snapshot.capture_state()?;
        let restore_result = restore_after_owned_step(&native_snapshot, &native_postimage);
        restore_result?;
        temporary_result?;

        let args = L::policy_repair_args(window_minutes, &exclusions, false).ok_or_else(|| {
            CoreError::System("pnpm policy settlement command is unavailable".to_string())
        })?;
        self.cmd
            .run(&project.root, &args)
            .await
            .map_err(propagate_repeated_minimum_age_rejection)
    }

    async fn configured_value<T>(&self, project: &Project, key: &str) -> Result<T>
    where
        T: DeserializeOwned + Default,
    {
        let args = vec![
            "config".to_string(),
            "get".to_string(),
            key.to_string(),
            "--json".to_string(),
        ];
        let output = self.cmd.stdout(&project.root, &args).await?;
        let value = output.trim();
        if value.is_empty() || value == "null" || value == "undefined" {
            return Ok(T::default());
        }
        serde_json::from_str(value)
            .map_err(|error| CoreError::Serialization(format!("pnpm {key}: {error}")))
    }
}

fn minimum_age_lock_rejected(error: &CoreError) -> bool {
    matches!(
        error,
        CoreError::Tool { stderr, .. }
            if stderr.contains("[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION]")
    )
}

/// Keeps a failed repair out of resilient apply's candidate-conflict isolation.
///
/// The retry already grants every known starting violation a narrow exemption.
/// Seeing the same preflight error again means the repair mechanism itself failed, not that one
/// planned version is unsatisfiable.
fn propagate_repeated_minimum_age_rejection(error: CoreError) -> CoreError {
    if minimum_age_lock_rejected(&error) {
        CoreError::System(format!(
            "pnpm minimum-release-age repair did not clear the starting-lock violation: {error}"
        ))
    } else {
        error
    }
}

/// pnpm stops at the first matching exclusion rule, so one package's exact versions must share a
/// `version||version` union rather than appearing as separate entries.
/// A targeted package excludes only its approved destination; allowing its rejected starting version
/// would let the settlement resolve float back to it after the temporary override is removed.
fn minimum_age_repair_exclusions(plan: &Plan, configured_exclusions: Vec<String>) -> Vec<String> {
    let mut exclusions = configured_exclusions.into_iter().collect::<BTreeSet<_>>();
    let targeted = plan
        .changes
        .iter()
        .map(|change| change.package.name.as_str())
        .collect::<HashSet<_>>();
    let mut exact_versions = BTreeMap::<String, BTreeSet<String>>::new();
    for violation in plan
        .baseline_violations
        .iter()
        .filter(|violation| !targeted.contains(violation.package.as_str()))
    {
        exact_versions
            .entry(violation.package.clone())
            .or_default()
            .insert(violation.version.to_string());
    }
    for change in &plan.changes {
        exact_versions
            .entry(change.package.name.clone())
            .or_default()
            .insert(change.to.to_string());
    }
    exclusions.extend(exact_versions.into_iter().map(|(package, versions)| {
        format!(
            "{package}@{}",
            versions.into_iter().collect::<Vec<_>>().join("||")
        )
    }));
    exclusions.into_iter().collect()
}

/// The post-resolve lock inconsistency, but only when this resolve introduced it.
///
/// A mismatch already present in the pre-apply lock (e.g. a pnpm `overrides` entry that legally pins
/// a direct dependency outside its declared range — `--frozen-lockfile` accepts that via the lock's
/// `overrides:` section, which the cheap check does not read) must not fail apply: every recovery
/// trial would fail identically and the run would misreport all candidates as held.
/// The before/after gate also absorbs any node-semver vs rust-semver divergence: whatever the check
/// misjudges, it misjudges identically on both sides.
fn new_lock_inconsistency<L: NodeLock>(before: Option<&str>, after: &str) -> Option<String> {
    let detail = L::lock_consistency_error(after)?;
    before
        .is_none_or(|content| L::lock_consistency_error(content).is_none())
        .then_some(detail)
}

/// Names workspace importers DECLARE on more than one distinct line — a genuine split that must be
/// skipped (exact-pinning one target would collapse the other line), unlike everything else which is
/// exact-pinned.
/// A name splits when importers resolve it to different versions (a v4/v5
/// split) OR declare it with different range specifiers (`~7.3.0` vs `^7.0.0`, `"<4"` vs `^4`) — the
/// latter even at one resolved version, since exact-pinning would still drag the narrower member off
/// its declared range.
///
/// Derived from per-importer declarations (`member_sources`), NOT the full resolved package set: a
/// direct dependency that merely shares a name with a transitive copy resolved at another version is
/// single-declared, so it stays exact-pinned — its per-package window and any out-of-range widen are
/// honored.
/// Counting the whole resolved graph instead would misclassify such a dep as multi-version and float
/// it, dropping the widen so a cross-major/out-of-range target can never land.
fn multi_version_names<L: NodeLock>(content: &str) -> HashSet<String> {
    L::member_sources(content).names_declared_at_multiple_versions()
}

#[async_trait]
impl<L: NodeLock> ToolWrite for NpmTool<L> {
    fn mutation_tool(&self) -> ToolId {
        L::ID
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        journal::<L>(project, plan)
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, journal) = mutation.parts_for(self)?;
        // The peer-feasibility gate runs against the journaled pre-apply lock, before any resolver
        // work: a cross-major target a still-present dependent's peer range excludes is held up
        // front — pnpm's resolver only *warns* on the mismatch, and npm (which rejects it by
        // default) commits it under relaxed enforcement.
        // The gated changes never reach the resolve (no manifest widen, no pin).
        // Workspace member manifests are read from disk here — still pre-apply state — because the
        // lock is not authoritative for a local package's peer contracts.
        let lock = journaled_lock::<L>(journal);
        let evidence = PeerEvidence::gather::<L>(Some(&project.root), lock);
        let PeerPartition {
            retained: plan,
            skipped: peer_held,
        } = partition_peer_held::<L>(plan, &evidence);
        // A manager with a native joint resolve (pnpm) re-resolves the whole importer graph
        // jointly (peer verification may reject candidates and re-resolve) and reports the full
        // before/after lock diff, so a candidate can never silently move another package and
        // mutually-exclusive peers settle at a single fixed point.
        // The others (npm/yarn/bun) lack a joint pin-set resolve, so they keep the per-package relock
        // path.
        let mut report = if L::supports_whole_graph_resolve() {
            self.apply_whole_graph(project, &plan, journal, &evidence.workspace)
                .await?
        } else {
            self.apply_per_package(project, &plan, journal, &evidence.workspace)
                .await?
        };
        report.skipped.extend(peer_held);
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        let before =
            absolute_cutoff_from_project(project.exclude_newer.as_deref(), jiff::Timestamp::now());
        self.cmd
            .verify(
                &project.root,
                &L::build_args(before.as_deref()),
                "install succeeded",
            )
            .await
    }

    async fn refresh_lock(&self, project: &Project) -> Result<Option<LockVerifyReport>> {
        let window_minutes =
            window_minutes_from_cutoff(project.exclude_newer.as_deref(), jiff::Timestamp::now());
        let Some(args) = L::refresh_lock_args(window_minutes) else {
            return Ok(None);
        };
        self.cmd
            .lock_report(&project.root, &args, &format!("{} refreshed", L::LOCKFILE))
            .await
            .map(Some)
    }

    fn supports_lock_refresh(&self) -> bool {
        L::supports_lock_refresh()
    }

    fn successful_apply_proves_lock_current(&self) -> bool {
        true
    }

    fn sync_scope(&self) -> SyncScope {
        // Only pnpm has a native min-age file, so only pnpm is project-scoped; the others sync nothing.
        if L::NATIVE_MIN_AGE_FILE.is_some() {
            SyncScope::Project
        } else {
            SyncScope::None
        }
    }

    async fn write_native(
        &self,
        project: &Project,
        policy: &ResolvedPolicy,
        dry_run: bool,
    ) -> Result<SyncReport> {
        let Some(file) = L::NATIVE_MIN_AGE_FILE else {
            return Ok(SyncReport::Unsupported); // npm/yarn/bun have no native cooldown knob
        };
        let path = project.root.join(file);
        let Some(minutes) = policy.default_window.as_ref().and_then(window_minutes) else {
            // pnpm's minimumReleaseAge is a rolling minute count; a freeze date or opt-out can't be
            // expressed, so leave the file untouched.
            return Ok(SyncReport::Unchanged { path });
        };
        let mut changed =
            set_yaml_scalar(&path, "minimumReleaseAge", &minutes.to_string(), dry_run)?;
        // The cooldown.toml `latest`/`allow` packages become pnpm's native per-package exemption list,
        // so a package cooldown's own policy exempts is also exempt from pnpm's rolling
        // minimumReleaseAge gate (otherwise the native window would still quarantine it).
        // An empty list removes the key, so toggling a package back under the cooldown cleans up
        // after itself.
        changed |= set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &policy.exempt_packages,
            dry_run,
        )?;
        Ok(if changed {
            SyncReport::Written { path }
        } else {
            SyncReport::Unchanged { path }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{Npm, Pnpm};
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use indoc::indoc;
    fn raw(version: &str) -> RawRelease {
        RawRelease {
            version: cooldown_core::Version::new(version),
            published_at: None,
            yanked: false,
            artifacts: Vec::new(),
        }
    }

    /// Only releases ordered strictly above the `latest`-tagged version are marked beyond the tag
    /// (the fumadocs-core shape: `17.0.0` above `latest = 16.13.0`).
    #[test]
    fn build_releases_marks_only_releases_above_the_latest_tag() {
        let releases = build_releases(
            "16.11.4",
            vec![raw("16.11.4"), raw("17.0.0"), raw("16.13.0")],
            Some("16.13.0"),
        );
        let beyond: Vec<&str> = releases
            .iter()
            .filter(|release| release.beyond_latest_tag)
            .map(|release| release.version.as_str())
            .collect();
        assert_eq!(beyond, vec!["17.0.0"]);
    }

    /// A tag naming a version absent from the release list (a registry inconsistency) fails open:
    /// nothing is marked, so no ceiling applies.
    /// The same applies when there is no tag.
    #[test]
    fn unknown_or_absent_latest_tag_marks_nothing() {
        for tag in [Some("99.0.0"), None] {
            let releases = build_releases("1.0.0", vec![raw("1.0.0"), raw("2.0.0")], tag);
            assert!(
                releases.iter().all(|release| !release.beyond_latest_tag),
                "tag {tag:?} must not mark any release"
            );
        }
    }

    /// A tag at the newest release marks nothing — every release is at or below it.
    #[test]
    fn tag_at_the_newest_release_marks_nothing() {
        let releases = build_releases(
            "1.0.0",
            vec![raw("1.0.0"), raw("1.1.0"), raw("2.0.0")],
            Some("2.0.0"),
        );
        assert!(releases.iter().all(|release| !release.beyond_latest_tag));
    }

    /// A prerelease ordered above the tag is marked too — harmless for the prerelease rule (quality
    /// already excludes it) but it keeps the marker's meaning uniform: "ordered above the tag".
    #[test]
    fn prerelease_above_the_tag_is_marked() {
        let releases = build_releases(
            "1.0.0",
            vec![raw("1.0.0"), raw("2.0.0"), raw("3.0.0-rc.1")],
            Some("2.0.0"),
        );
        let beyond: Vec<&str> = releases
            .iter()
            .filter(|release| release.beyond_latest_tag)
            .map(|release| release.version.as_str())
            .collect();
        assert_eq!(beyond, vec!["3.0.0-rc.1"]);
    }

    #[test]
    fn whole_graph_args_pins_each_per_package_target_only_for_pnpm() {
        // pnpm pins each planned candidate to its EXACT per-package target in one joint resolve, so a
        // stricter-windowed package lands at its own (possibly older) target rather than overshooting.
        // The window rides inline as `minimumReleaseAge`, the floor for any fresh transitive the pins
        // drag in.
        // Each exact pin becomes `name@target`.
        // Multi-version candidates stay out of this command before construction because bare
        // `pnpm update <name>` can write an out-of-range lock entry while `--no-save` leaves the
        // manifest unchanged.
        // Importer filters cover both root and member declarations without running the command in
        // unrelated workspace packages.
        let pins = vec![
            ("eslint".to_string(), "9.5.0".to_string()),
            (
                "@typescript-eslint/eslint-plugin".to_string(),
                "8.0.0".to_string(),
            ),
        ];
        let filters = vec![".".to_string(), "./packages/app".to_string()];
        assert_eq!(
            Pnpm::whole_graph_args(&pins, &filters, Some(20160)),
            Some(vec![
                "--filter".to_string(),
                ".".to_string(),
                "--filter".to_string(),
                "./packages/app".to_string(),
                "--fail-if-no-match".to_string(),
                "update".to_string(),
                "eslint@9.5.0".to_string(),
                "@typescript-eslint/eslint-plugin@8.0.0".to_string(),
                "--lockfile-only".to_string(),
                "--no-save".to_string(),
                "--config.minimumReleaseAge=20160".to_string(),
            ])
        );
        assert_eq!(
            Pnpm::whole_graph_args(&pins, &[], None),
            Some(vec![
                "update".to_string(),
                "--recursive".to_string(),
                "eslint@9.5.0".to_string(),
                "@typescript-eslint/eslint-plugin@8.0.0".to_string(),
                "--lockfile-only".to_string(),
                "--no-save".to_string(),
            ])
        );
        let exclusions = [
            "eslint@9.4.0".to_string(),
            "@typescript-eslint/*".to_string(),
        ];
        assert_eq!(
            Pnpm::policy_repair_args(Some(20160), &exclusions, true),
            Some(vec![
                "install".to_string(),
                "--lockfile-only".to_string(),
                "--resolution-only".to_string(),
                "--trust-lockfile".to_string(),
                "--config.minimumReleaseAge=20160".to_string(),
                "--config.minimumReleaseAgeExclude=eslint@9.4.0".to_string(),
                "--config.minimumReleaseAgeExclude=@typescript-eslint/*".to_string(),
            ])
        );
        assert_eq!(
            Pnpm::policy_repair_args(None, &[], false),
            Some(vec![
                "install".to_string(),
                "--lockfile-only".to_string(),
                "--trust-lockfile".to_string(),
            ])
        );
        assert_eq!(
            Npm::policy_repair_args(Some(20160), &exclusions, true),
            None
        );
        assert_eq!(Pnpm::whole_graph_args(&[], &filters, None), None);
        // npm/yarn/bun have no joint resolve, so they keep the per-package path.
        assert!(!Npm::supports_whole_graph_resolve());
        assert!(Pnpm::supports_whole_graph_resolve());
        assert_eq!(Npm::whole_graph_args(&pins, &filters, Some(20160)), None);
        assert_eq!(
            crate::lock::Yarn::whole_graph_args(&pins, &filters, None),
            None
        );
        assert_eq!(
            crate::lock::Bun::whole_graph_args(&[], &filters, None),
            None
        );
    }

    #[test]
    fn minimum_age_repair_exclusions_are_exact_and_deterministic() {
        let plan = Plan {
            changes: vec![change("eslint", "10.7.0", "10.6.0")],
            baseline_violations: vec![
                cooldown_core::BaselineViolation {
                    package: "eslint".to_string(),
                    version: Version::new("10.7.0"),
                },
                cooldown_core::BaselineViolation {
                    package: "flatted".to_string(),
                    version: Version::new("3.4.3"),
                },
                cooldown_core::BaselineViolation {
                    package: "flatted".to_string(),
                    version: Version::new("3.4.2"),
                },
            ],
            ..Plan::default()
        };

        assert_eq!(
            minimum_age_repair_exclusions(
                &plan,
                vec!["@typescript-eslint/*".to_string(), "nanoid".to_string()],
            ),
            vec![
                "@typescript-eslint/*".to_string(),
                "eslint@10.6.0".to_string(),
                "flatted@3.4.2||3.4.3".to_string(),
                "nanoid".to_string(),
            ]
        );
    }

    #[test]
    fn repeated_minimum_age_rejection_is_not_a_resolver_conflict() {
        let error = CoreError::Tool {
            tool: "pnpm".to_string(),
            termination: cooldown_core::ToolTermination::ExitCode(1),
            stderr: "[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] lock rejected".to_string(),
        };

        let propagated = propagate_repeated_minimum_age_rejection(error);

        assert!(propagated.is_local_environment_failure());
        assert!(
            propagated
                .to_string()
                .contains("repair did not clear the starting-lock violation")
        );

        let conflict = CoreError::Tool {
            tool: "pnpm".to_string(),
            termination: cooldown_core::ToolTermination::ExitCode(1),
            stderr: "unresolvable peer dependency".to_string(),
        };
        assert!(
            !propagate_repeated_minimum_age_rejection(conflict).is_local_environment_failure(),
            "ordinary resolver failures must remain eligible for candidate isolation"
        );
    }

    #[test]
    fn pnpm_importer_filters_use_portable_location_syntax() {
        assert_eq!(pnpm_location_filter("."), ".");
        assert_eq!(pnpm_location_filter("pkgs/app"), "./pkgs/app");
        assert_eq!(
            pnpm_location_filter("pkgs/space app/[test]/quo'te"),
            "./pkgs/space app/[test]/quo'te"
        );
    }

    #[test]
    fn locked_versions_keeps_the_newest_copy_of_a_duplicated_name() {
        let lock = "lockfileVersion: '9.0'\n\npackages:\n\n  foo@1.0.0:\n    resolution: {integrity: sha512-a}\n\n  foo@2.0.0:\n    resolution: {integrity: sha512-b}\n\n  bar@3.1.0:\n    resolution: {integrity: sha512-c}\n";
        let versions = locked_versions::<Pnpm>(lock);
        assert_eq!(versions.get("foo").map(String::as_str), Some("2.0.0"));
        assert_eq!(versions.get("bar").map(String::as_str), Some("3.1.0"));
    }

    #[test]
    fn lock_inconsistency_pre_existing_before_the_resolve_is_not_charged_to_it() {
        // `vite` resolved at 7.3.5 against a `^6` specifier — inconsistent.
        let stale = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/admin:
                dependencies:
                  vite:
                    specifier: ^6
                    version: 7.3.5(@types/node@22.19.20)
        "};
        // Same importer, consistent.
        let clean = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/admin:
                dependencies:
                  vite:
                    specifier: ^6
                    version: 6.4.3(@types/node@22.19.20)
        "};

        // The resolve introduced the mismatch (clean or absent before) -> surfaces.
        assert!(new_lock_inconsistency::<Pnpm>(Some(clean), stale).is_some());
        assert!(new_lock_inconsistency::<Pnpm>(None, stale).is_some());
        // The mismatch predates the resolve (e.g. an overrides-pinned direct dep) -> suppressed.
        assert_eq!(new_lock_inconsistency::<Pnpm>(Some(stale), stale), None);
        // A consistent after-lock is never an error.
        assert_eq!(new_lock_inconsistency::<Pnpm>(Some(stale), clean), None);
    }

    #[test]
    fn reached_respects_move_direction() {
        // These changes carry no members, so `reached` falls back to the name's newest copy.
        let members = crate::lock::MemberIndex::default();
        let mut after = HashMap::new();
        after.insert("pkg-a".to_string(), "2.0.0".to_string());
        let forward = change("pkg-a", "1.0.0", "2.0.0");
        assert!(reached(&after, &members, &forward));
        let forward_short = change("pkg-a", "1.0.0", "2.1.0");
        assert!(!reached(&after, &members, &forward_short));
        let mut down = change("pkg-a", "3.0.0", "2.0.0");
        down.downgrade = true;
        assert!(reached(&after, &members, &down));
        let mut down_short = change("pkg-a", "3.0.0", "1.0.0");
        down_short.downgrade = true;
        assert!(!reached(&after, &members, &down_short));
    }

    #[test]
    fn reached_checks_the_declaring_member_not_the_names_newest_copy() {
        // A multi-version dependency has `pkgs/low` on the v22 line and `pkgs/high` on v25.
        // A candidate bumping `pkgs/low` to 25 must be judged at `pkgs/low`'s own copy (still 22),
        // not the name's newest copy (25, owned by `pkgs/high`), which would falsely report it
        // landed.
        let lock = "\
importers:

  pkgs/low:
    dependencies:
      '@types/node':
        specifier: ^22.0.0
        version: 22.19.20

  pkgs/high:
    dependencies:
      '@types/node':
        specifier: ^25.0.0
        version: 25.9.2

packages:

  '@types/node@22.19.20':
    resolution: {integrity: sha512-a}
  '@types/node@25.9.2':
    resolution: {integrity: sha512-b}
";
        let after_members = Pnpm::member_sources(lock);
        let after_newest = locked_versions::<Pnpm>(lock);
        assert_eq!(
            after_newest.get("@types/node").map(String::as_str),
            Some("25.9.2")
        );

        let mut low = change("@types/node", "22.19.20", "25.9.2");
        low.members = vec![MemberRef {
            name: "low".to_string(),
            path: "pkgs/low".to_string(),
        }];
        assert!(
            !reached(&after_newest, &after_members, &low),
            "the v22 member did not reach 25 even though the name's newest copy is 25"
        );

        let mut high = change("@types/node", "25.0.0", "25.9.2");
        high.members = vec![MemberRef {
            name: "high".to_string(),
            path: "pkgs/high".to_string(),
        }];
        assert!(
            reached(&after_newest, &after_members, &high),
            "the v25 member's own copy is at the target"
        );
    }

    #[test]
    fn collateral_change_marks_a_forced_regression_as_a_downgrade() {
        let down = collateral_change::<Pnpm>("shared", "2.0.1", "1.4.0");
        assert_eq!(down.package.name, "shared");
        assert!(down.downgrade);
        assert!(!down.direct);
        let up = collateral_change::<Pnpm>("shared", "1.4.0", "2.0.1");
        assert!(!up.downgrade);
    }

    #[test]
    fn collateral_changes_surface_a_held_candidates_real_movement() {
        let before = HashMap::from([("shared".to_string(), "1.4.0".to_string())]);
        let after = HashMap::from([("shared".to_string(), "1.4.3".to_string())]);

        // A held planned candidate has no applied row, yet the resolve still floated it off its
        // baseline.
        // That net move must surface as a collateral row beside the held skip instead of being
        // silently dropped behind the planned name.
        let collateral = collateral_changes::<Pnpm>(&before, &after, &[]);
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "shared");
        assert_eq!(collateral[0].from.as_str(), "1.4.0");
        assert_eq!(collateral[0].to.as_str(), "1.4.3");

        // An applied row claiming a *different* landing (a directional overshoot the executor
        // re-verifies into a skip) does not mask the movement; a row landing exactly there does —
        // even when its planned `from` is a stale duplicate copy's baseline, not the newest copy's.
        let overshoot = [change("shared", "1.4.0", "1.4.1")];
        assert_eq!(
            collateral_changes::<Pnpm>(&before, &after, &overshoot).len(),
            1
        );
        let stale_duplicate_baseline = [change("shared", "1.3.9", "1.4.3")];
        assert!(collateral_changes::<Pnpm>(&before, &after, &stale_duplicate_baseline).is_empty());
    }

    #[tokio::test]
    async fn write_native_writes_minimum_release_age_exclude_for_latest_packages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(root.join("pnpm-workspace.yaml"), "packages:\n  - \"a\"\n").expect("write");
        let project = Project {
            root: root.clone(),
            kind: crate::lock::Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let policy = cooldown_core::ResolvedPolicy {
            default_window: Some(cooldown_core::WindowSpec::MinAge(
                jiff::SignedDuration::from_hours(24 * 14),
            )),
            exempt_packages: vec!["@typescript/native-preview".to_string()],
        };

        let tool = NpmTool::<crate::lock::Pnpm>::from_http(
            SharedHttp::new(
                tempfile::tempdir().expect("cache").path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        let report = ToolWrite::write_native(&tool, &project, &policy, false)
            .await
            .expect("sync");
        std::assert_matches!(report, cooldown_core::SyncReport::Written { .. });
        let written = std::fs::read_to_string(root.join("pnpm-workspace.yaml")).expect("read");
        assert!(
            written.contains("minimumReleaseAge: 20160"),
            "window synced"
        );
        assert!(
            written.contains("minimumReleaseAgeExclude:\n  - \"@typescript/native-preview\""),
            "latest package exempted natively: {written}"
        );
    }

    fn tool() -> NpmTool<Npm> {
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        NpmTool::from_http(
            SharedHttp::new(cache_dir.path(), cooldown_registry::HttpOptions::default())
                .expect("http"),
        )
    }

    #[tokio::test]
    async fn dependencies_split_direct_from_transitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "lodash": "4.17.15" } }"#,
        )
        .expect("write manifest");
        let lock_json = indoc! {r#"
            {
                "lockfileVersion": 3,
                "packages": {
                    "": { "version": "0.1.0", "dependencies": { "lodash": "4.17.15" } },
                    "node_modules/lodash": { "version": "4.17.15" },
                    "node_modules/ms": { "version": "2.1.3" }
                }
            }"#};
        std::fs::write(root.join("package-lock.json"), lock_json).expect("write lock");
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };

        let direct = tool()
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct deps");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].package.name, "lodash");
        assert!(direct[0].direct);
        assert_eq!(direct[0].package.registry.as_deref(), Some(NPM));

        let graph = tool()
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph deps");
        assert_eq!(graph.len(), 2); // lodash (direct) + ms (transitive)
        assert!(graph.iter().any(|d| d.package.name == "ms" && !d.direct));
    }

    #[tokio::test]
    async fn npm_v1_lock_falls_back_to_root_manifest_directness() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "lodash": "4.17.15" } }"#,
        )
        .expect("write manifest");
        let lock_json = indoc! {r#"
            {
                "lockfileVersion": 1,
                "dependencies": {
                    "lodash": { "version": "4.17.15" },
                    "ms": { "version": "2.1.3" }
                }
            }"#};
        std::fs::write(root.join("package-lock.json"), lock_json).expect("write lock");
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };

        let direct = tool()
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct deps");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].package.name, "lodash");
        assert!(
            direct[0].members.is_empty(),
            "v1 locks have no member attribution"
        );
    }

    fn pnpm_tool() -> NpmTool<crate::lock::Pnpm> {
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        NpmTool::from_http(
            SharedHttp::new(cache_dir.path(), cooldown_registry::HttpOptions::default())
                .expect("http"),
        )
    }

    #[tokio::test]
    async fn pnpm_directness_is_version_exact() {
        // An importer declares `foo@2.0.0`; `foo@1.0.0` is only a transitive copy in the graph.
        // Direct-ness must be version-exact: only the declared 2.0.0 is direct (and attributed),
        // and the transitive 1.0.0 is never reported as a direct dependency with a blank source.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("apps/a")).expect("mkdir");
        std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#).expect("root manifest");
        std::fs::write(
            root.join("apps/a/package.json"),
            r#"{ "name": "@x/a", "dependencies": { "foo": "2.0.0" } }"#,
        )
        .expect("member manifest");
        std::fs::write(
            root.join("pnpm-lock.yaml"),
            "lockfileVersion: '9.0'\n\nimporters:\n\n  apps/a:\n    dependencies:\n      foo:\n        specifier: 2.0.0\n        version: 2.0.0\n\npackages:\n\n  foo@1.0.0:\n    resolution: {integrity: sha512-x}\n\n  foo@2.0.0:\n    resolution: {integrity: sha512-y}\n",
        )
        .expect("write lock");
        let project = Project {
            root: root.clone(),
            kind: crate::lock::Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };

        let direct = pnpm_tool()
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct deps");
        assert_eq!(
            direct.len(),
            1,
            "only the importer-declared version is direct"
        );
        assert_eq!(direct[0].current.as_str(), "2.0.0");
        assert_eq!(
            direct[0]
                .members
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["@x/a"],
            "the declared version is attributed to its importer by package name"
        );

        // In graph scope both copies appear, but only 2.0.0 is marked direct.
        let graph = pnpm_tool()
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph deps");
        assert_eq!(graph.len(), 2);
        let transitive = graph
            .iter()
            .find(|d| d.current.as_str() == "1.0.0")
            .expect("1.0.0 present in graph");
        assert!(!transitive.direct, "the transitive copy is not direct");
        assert!(
            transitive.members.is_empty(),
            "and has no source attribution"
        );
    }

    #[tokio::test]
    async fn mutation_journal_restores_manifest_and_lock() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("package.json"), "{\"name\":\"demo\"}")?;
        std::fs::write(root.join("package-lock.json"), "{\"original\":true}")?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };

        let captured = tool()
            .mutation_journal(
                &project,
                &Plan {
                    changes: vec![change("nanoid", "3.1.0", "3.3.0")],
                    rewrite: RewriteMode::Auto,
                    ..Plan::default()
                },
            )
            .await?;
        std::fs::write(root.join("package.json"), "{\"mutated\":true}")?;
        std::fs::write(root.join("package-lock.json"), "{\"mutated\":true}")?;
        captured.restore()?;

        let restored_manifest = std::fs::read_to_string(root.join("package.json"))?;
        assert_eq!(restored_manifest, "{\"name\":\"demo\"}");
        let restored = std::fs::read_to_string(root.join("package-lock.json"))?;
        assert_eq!(restored, "{\"original\":true}");
        Ok(())
    }

    #[tokio::test]
    async fn mutation_journal_restores_member_manifests() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir_all(root.join("apps/a"))?;
        std::fs::write(root.join("package.json"), "{\"name\":\"root\"}")?;
        std::fs::write(
            root.join("apps/a/package.json"),
            r#"{ "dependencies": { "nanoid": "^3.0.0" } }"#,
        )?;
        std::fs::write(root.join("package-lock.json"), "{\"original\":true}")?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut planned = change("nanoid", "3.1.0", "3.3.0");
        planned.members = vec![MemberRef {
            name: "a".into(),
            path: "apps/a".into(),
        }];

        let captured = tool()
            .mutation_journal(
                &project,
                &Plan {
                    changes: vec![planned],
                    rewrite: RewriteMode::Always,
                    ..Plan::default()
                },
            )
            .await?;
        std::fs::write(root.join("apps/a/package.json"), "{\"mutated\":true}")?;
        captured.restore()?;

        let restored = std::fs::read_to_string(root.join("apps/a/package.json"))?;
        assert_eq!(restored, r#"{ "dependencies": { "nanoid": "^3.0.0" } }"#);
        Ok(())
    }

    fn change(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(Npm::ID, name, Some(NPM.to_string())),
            from: Version::new(from),
            to: Version::new(to),
            kind: cooldown_core::UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    #[tokio::test]
    async fn apply_skips_when_no_declaring_manifest_entry_exists() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#)?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let plan = Plan {
            changes: vec![change("nanoid", "3.1.0", "3.3.0")],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };

        let tool = tool();
        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;

        assert!(report.applied.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::NotEligible);
        let manifest = std::fs::read_to_string(root.join("package.json"))?;
        assert_eq!(manifest, r#"{ "name": "root" }"#);
        Ok(())
    }
}
