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
    Project, ProjectMarker, ProjectMutationJournal, RawRelease, Release, ReleaseFetcher,
    ReleaseOrder, ReleaseQuality, ResolvedPolicy, Result, RewriteMode, SkipReason, Skipped,
    SyncReport, SyncScope, ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
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
    let mut files = Vec::with_capacity(rels.len());
    for rel in rels {
        files.push(ProjectMutationJournal::capture_file(&project.root, &rel)?);
    }
    ProjectMutationJournal::new(files)
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
    /// transitives. Diffing the journaled lock against the final lock keeps those movements visible.
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
            // succeeded and makes the outer batch committable. Capture the state after those earlier
            // successes so this candidate can be restored independently.
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
                    restore_after_owned_step(
                        &candidate_journal,
                        &project.root,
                        &attempt.postimage,
                    )?;
                    report.skipped.push(Skipped {
                        change: change.clone(),
                        reason: SkipReason::ResolverConflict,
                        offending: Some(change.package.clone()),
                        detail: None,
                    });
                }
                Err(error) if error.is_local_environment_failure() => {
                    restore_after_owned_step(
                        &candidate_journal,
                        &project.root,
                        &attempt.postimage,
                    )?;
                    return Err(error);
                }
                Err(error) => {
                    restore_after_owned_step(
                        &candidate_journal,
                        &project.root,
                        &attempt.postimage,
                    )?;
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
    /// cargo/go pattern ported to pnpm. Peer verification can reject candidates and re-resolve,
    /// so one apply may run several native resolves (see [`Self::resolve_and_verify_peers`]).
    ///
    /// One importer-filtered `pnpm update <pkg>@<target> … --lockfile-only --no-save` jointly
    /// re-resolves the affected graph, settling mutually-exclusive peer conflicts at a single fixed
    /// point instead of ping-ponging between per-package updates. Each `<pkg>@<target>` is the
    /// candidate's own `change.to`, computed by cooldown-core under that package's window, so a package
    /// with a *stricter* per-package window lands at its older per-package target rather than
    /// overshooting onto the global-window-newest — the gap a bare `--latest` left, since pnpm's
    /// `minimumReleaseAge` is a single global knob with no per-package publish-date cutoff. This mirrors
    /// cargo's `update --precise <to>` and go's `get module@<to>`: the per-package target already
    /// encodes the per-package window, so pinning it enforces that window exactly.
    ///
    /// `minimumReleaseAge` is passed as the *transitive* floor. A persisted native policy can reject
    /// an older lock before applying the exact pins that would repair it. For that migration state,
    /// pnpm is rerun with temporary exact overrides for the planned targets and `--trust-lockfile`;
    /// this skips only the rejected starting-lock preflight while the age floor still governs the
    /// replacement graph. The original native config is restored before pnpm settles the lock again.
    ///
    /// The report is the diff of the journal's pre-apply lock against the result, so every planned
    /// candidate is reported reached or held (naming the conflicting peer where attributable) and
    /// every collateral move of an unplanned package surfaces as its own row. A resolver failure
    /// after the repair retry marks the conflicting candidates held. The accepted result is
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

        // The pre-apply lock, captured in the journal. Both the newest-version map (for the move diff)
        // and the multi-version set (for the exact-pin-vs-float decision) are derived from this one
        // copy — no extra disk read, and both see exactly the lock the resolve starts from.
        let before_content = journaled_lock::<L>(journal);
        let before = before_content.map(locked_versions::<L>).unwrap_or_default();
        let multi_version = before_content
            .map(multi_version_names::<L>)
            .unwrap_or_default();

        // pnpm's `minimumReleaseAge` is a *rolling* age, so the cutoff is realized against the current
        // instant. An absolute `--freeze` cutoff becomes `now - freeze` minutes — equivalent to the
        // freeze date as long as the same `now` governs both the seed and this resolve (it does:
        // wall-clock advances only seconds between them, far below the day-scale window under test). It
        // is passed only as the *transitive* floor here; each planned candidate is pinned to its exact
        // per-package target, so its own (possibly stricter) window is enforced by the pin, not this cap.
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
            // Whether the lock's version for this name actually moved. A name can resolve to several
            // copies in a pnpm graph; `before`/`after` track its *newest* copy, so a candidate planned
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
                // Reached its target without a net lock move — already satisfied (a duplicate copy of
                // the same name is at the target). A no-op, neither applied nor held.
            } else if multi_version.contains(name) {
                // A dependency declared at multiple versions across the workspace is deliberately kept
                // in range, not pinned to the target. That is a conservative hold, not a resolver
                // conflict, and it must not be advertised as adoptable: `outdated`'s verify
                // reclassifies it blocked.
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::MultiVersionHeld,
                    offending: None,
                    detail: None,
                });
            } else {
                // The joint resolve could not place this candidate at its target without breaking the
                // lock — a mutually-exclusive peer won. Name the sibling whose peer choice excluded it
                // so the report says "held: conflicts with <pkg>"; absent a unique blocker it falls
                // back to the candidate itself (the generic "resolver rejected" form).
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

        // The hard requirement: no net version change to *any* package may be omitted. Every moved
        // package the applied rows above do not already report is surfaced as its own collateral
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
    /// caller's bisect and extra rounds correspond only to real cascades. An unattributable
    /// violation propagates as a non-local rejection for candidate isolation. The rounds are
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
            let mut resolve = OwnedStep::capture(resolve_result, journal, &project.root)?;
            match resolve.result {
                Ok(()) => {}
                Err(error) if error.is_local_environment_failure() => {
                    restore_after_owned_step(journal, &project.root, &resolve.postimage)?;
                    return Err(error);
                }
                Err(error)
                    if minimum_age_lock_rejected(&error)
                        && window_minutes.is_some_and(|minutes| minutes > 0)
                        && L::NATIVE_MIN_AGE_FILE.is_some() =>
                {
                    // A persisted minimumReleaseAge validates the starting lock before pnpm
                    // applies the exact pins. Restore any partial resolver work, then rebuild
                    // through temporary exact overrides while retaining the age floor.
                    restore_after_owned_step(journal, &project.root, &resolve.postimage)?;
                    self.repair_policy_rejected_graph(
                        project,
                        &active,
                        multi_version,
                        window_minutes,
                    )
                    .await?;
                    resolve.postimage = journal.capture_state(&project.root)?;
                }
                // The joint resolve is unsatisfiable as a whole. Propagate the failure so the
                // caller's `apply_resilient` can isolate the offending candidate(s) (an
                // unfetchable version, one side of a conflict) and apply the rest, instead of
                // holding every candidate. The caller restores the journal, so no partial lock
                // is kept.
                Err(error) => {
                    restore_after_owned_step(journal, &project.root, &resolve.postimage)?;
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
            restore_after_owned_step(journal, &project.root, &resolve.postimage)?;
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
    /// stricter-than-global per-package window with no overshoot. A candidate a member declares at a
    /// version *other* members also hold at a different version (a v4/v5 split, which pnpm keeps like
    /// cargo) is skipped instead: exact-pinning one target would collapse every other copy onto it, and
    /// pnpm's bare `update <name>` can write an out-of-range lock entry while `--no-save` leaves the
    /// manifest unchanged. The pre-apply lock identifies those multi-version names; a missing/unparsable
    /// lock means nothing is multi-version yet, so every pin is exact.
    ///
    /// Widen is for the exact pins only, and only when their target is out of the declared range
    /// (`Auto`) or always (`Always`). It is mandatory there: `pnpm update <pkg>@<target> --no-save`
    /// re-pins the lock to an out-of-range target but leaves the manifest as written, so the next
    /// resolve (which re-resolves any package it is not pinning, against its manifest range) snaps the
    /// candidate back into range and breaks the fixed point. A multi-version candidate is never widened
    /// — widening would let it cross its own range boundary, the very line we are preserving.
    ///
    /// Each declaring member becomes a pnpm portable location filter. This reaches root and member
    /// importers without relying on package names or running the update in unrelated workspace
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
        // held. No post-resolve re-widen loop is needed.
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
                // Preserve every distinct line. A bare pnpm update can write an out-of-range lock
                // entry while leaving package.json untouched.
                continue;
            }
            // Exact-pin: widen the owning manifest when the target is out of range so the exact lock
            // pin stays consistent with `package.json`. A candidate not declared in any owning manifest
            // (`target_in_declared_range` returns `false`) is widened too, so the pin is never left
            // dangling against a range that excludes it.
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
        let native_snapshot =
            ProjectMutationJournal::new(vec![ProjectMutationJournal::capture_file(
                &project.root,
                &native_rel,
            )?])?;
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
        let native_postimage = native_snapshot.capture_state(&project.root)?;
        let restore_result =
            restore_after_owned_step(&native_snapshot, &project.root, &native_postimage);
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
/// The retry already grants every known starting violation a narrow exemption. Seeing the same
/// preflight error again means the repair mechanism itself failed, not that one planned version is
/// unsatisfiable.
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
/// `version||version` union rather than appearing as separate entries. A targeted package excludes
/// only its approved destination; allowing its rejected starting version would let the settlement
/// resolve float back to it after the temporary override is removed.
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
/// trial would fail identically and the run would misreport all candidates as held. The before/after
/// gate also absorbs any node-semver vs rust-semver divergence: whatever the check misjudges, it
/// misjudges identically on both sides.
fn new_lock_inconsistency<L: NodeLock>(before: Option<&str>, after: &str) -> Option<String> {
    let detail = L::lock_consistency_error(after)?;
    before
        .is_none_or(|content| L::lock_consistency_error(content).is_none())
        .then_some(detail)
}

/// Names workspace importers DECLARE on more than one distinct line — a genuine split that must be
/// skipped (exact-pinning one target would collapse the other line), unlike everything else which is
/// exact-pinned. A name splits when importers resolve it to different versions (a v4/v5
/// split) OR declare it with different range specifiers (`~7.3.0` vs `^7.0.0`, `"<4"` vs `^4`) — the
/// latter even at one resolved version, since exact-pinning would still drag the narrower member off
/// its declared range.
///
/// Derived from per-importer declarations (`member_sources`), NOT the full resolved package set: a
/// direct dependency that merely shares a name with a transitive copy resolved at another version is
/// single-declared, so it stays exact-pinned — its per-package window and any out-of-range widen are
/// honored. Counting the whole resolved graph instead would misclassify such a dep as multi-version
/// and float it, dropping the widen so a cross-major/out-of-range target can never land.
fn multi_version_names<L: NodeLock>(content: &str) -> HashSet<String> {
    L::member_sources(content).names_declared_at_multiple_versions()
}

#[async_trait]
impl<L: NodeLock> ToolWrite for NpmTool<L> {
    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        journal::<L>(project, plan)
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        // The peer-feasibility gate runs against the journaled pre-apply lock, before any resolver
        // work: a cross-major target a still-present dependent's peer range excludes is held up
        // front — pnpm's resolver only *warns* on the mismatch, and npm (which rejects it by
        // default) commits it under relaxed enforcement. The gated changes never reach the
        // resolve (no manifest widen, no pin). Workspace member manifests are read from disk here
        // — still pre-apply state — because the lock is not authoritative for a local package's
        // peer contracts.
        let lock = journaled_lock::<L>(journal);
        let evidence = PeerEvidence::gather::<L>(Some(&project.root), lock);
        let PeerPartition {
            retained: plan,
            skipped: peer_held,
        } = partition_peer_held::<L>(plan, &evidence);
        // A manager with a native joint resolve (pnpm) re-resolves the whole importer graph
        // jointly (peer verification may reject candidates and re-resolve) and reports the full
        // before/after lock diff, so a candidate can never silently move
        // another package and mutually-exclusive peers settle at a single fixed point. The others
        // (npm/yarn/bun) lack a joint pin-set resolve, so they keep the per-package relock path.
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
        // minimumReleaseAge gate (otherwise the native window would still quarantine it). An empty list
        // removes the key, so toggling a package back under the cooldown cleans up after itself.
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
    use crate::apply::landing::{
        CandidateCommand, lockonly_command, preserving_pin, without_before,
    };
    use crate::lock::{Npm, Pnpm};
    use crate::peers::{first_new_peer_violation, workspace_peer_hold};
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use indoc::{formatdoc, indoc};
    use std::sync::Mutex;

    #[test]
    fn candidate_restore_refuses_drift_after_the_owned_command() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let relative = Utf8Path::new("package-lock.json");
        std::fs::write(root.join(relative), "original")?;
        let journal = ProjectMutationJournal::new(vec![ProjectMutationJournal::capture_file(
            root, relative,
        )?])?;
        std::fs::write(root.join(relative), "candidate")?;
        let postimage = journal.capture_state(root)?;
        std::fs::write(root.join(relative), "independent")?;

        let result = restore_after_owned_step(&journal, root, &postimage);

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        assert_eq!(std::fs::read_to_string(root.join(relative))?, "independent");
        Ok(())
    }

    struct CutoffFallbackCommand {
        authorized_manifest: String,
        pin_saves_manifest: bool,
        calls: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl CandidateCommand for CutoffFallbackCommand {
        async fn run_candidate(&self, root: &Utf8Path, args: &[String]) -> Result<()> {
            let call_index = {
                let mut calls = self.calls.lock().map_err(|_| {
                    CoreError::Filesystem("candidate command call log was poisoned".to_string())
                })?;
                let index = calls.len();
                calls.push(args.to_vec());
                index
            };
            let manifest = root.join("package.json");
            match call_index {
                0 => {
                    std::fs::write(&manifest, r#"{"range":"first-attempt-save"}"#)?;
                    Err(CoreError::Tool {
                        tool: "npm".to_string(),
                        termination: cooldown_core::ToolTermination::ExitCode(1),
                        stderr: "historical tree unavailable".to_string(),
                    })
                }
                1 => {
                    let live = std::fs::read_to_string(&manifest)?;
                    if live != self.authorized_manifest {
                        return Err(CoreError::LockConflict(format!(
                            "fallback saw `{live}` instead of the authorized manifest"
                        )));
                    }
                    if self.pin_saves_manifest {
                        std::fs::write(&manifest, r#"{"range":"fallback-pin-save"}"#)?;
                    }
                    Ok(())
                }
                2 => {
                    let live = std::fs::read_to_string(&manifest)?;
                    if live != self.authorized_manifest {
                        return Err(CoreError::LockConflict(format!(
                            "resync saw `{live}` instead of the authorized manifest"
                        )));
                    }
                    Ok(())
                }
                _ => Err(CoreError::LockConflict(
                    "candidate landing ran more commands than expected".to_string(),
                )),
            }
        }
    }

    fn retry_journals(
        root: &Utf8Path,
        authorized_manifest: &str,
    ) -> eyre::Result<(ProjectMutationJournal, ProjectMutationJournal)> {
        let relative = Utf8Path::new("package.json");
        std::fs::write(root.join(relative), r#"{"range":"baseline"}"#)?;
        let candidate = ProjectMutationJournal::new(vec![ProjectMutationJournal::capture_file(
            root, relative,
        )?])?;
        std::fs::write(root.join(relative), authorized_manifest)?;
        let authorized = ProjectMutationJournal::new(vec![ProjectMutationJournal::capture_file(
            root, relative,
        )?])?;
        Ok((candidate, authorized))
    }

    fn fallback_project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            manifest: root.join("package.json"),
            kind: Npm::ID,
            exclude_newer: None,
        }
    }

    #[tokio::test]
    async fn direct_cutoff_fallback_reapplies_the_authorized_manifest() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let authorized_manifest = r#"{"range":"authorized"}"#;
        let (candidate_journal, authorized_manifests) = retry_journals(root, authorized_manifest)?;
        let landing = CandidateLanding::Direct {
            command: vec!["install".to_string(), "--before=2026-01-01".to_string()],
            authorized_manifests,
        };
        let command = CutoffFallbackCommand {
            authorized_manifest: authorized_manifest.to_string(),
            pin_saves_manifest: false,
            calls: Mutex::new(Vec::new()),
        };

        let attempt = run_candidate_landing_with(
            &command,
            &fallback_project(root),
            &candidate_journal,
            &landing,
        )
        .await?;
        attempt.result?;

        assert_eq!(
            std::fs::read_to_string(root.join("package.json"))?,
            authorized_manifest
        );
        let calls = command
            .calls
            .lock()
            .map_err(|_| eyre::eyre!("candidate command call log was poisoned"))?;
        assert_eq!(calls.len(), 2);
        let fallback = calls
            .get(1)
            .ok_or_else(|| eyre::eyre!("fallback command was not recorded"))?;
        assert!(fallback.iter().all(|arg| !arg.starts_with("--before=")));
        Ok(())
    }

    #[tokio::test]
    async fn pin_cutoff_fallback_restores_the_final_attempt_before_resync() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let authorized_manifest = r#"{"range":"authorized"}"#;
        let (candidate_journal, authorized_manifests) = retry_journals(root, authorized_manifest)?;
        let landing = CandidateLanding::PinRestoreResync {
            pin: vec!["install".to_string(), "--before=2026-01-01".to_string()],
            authorized_manifests,
            resync: vec!["install".to_string(), "--before=2026-01-01".to_string()],
        };
        let command = CutoffFallbackCommand {
            authorized_manifest: authorized_manifest.to_string(),
            pin_saves_manifest: true,
            calls: Mutex::new(Vec::new()),
        };

        let attempt = run_candidate_landing_with(
            &command,
            &fallback_project(root),
            &candidate_journal,
            &landing,
        )
        .await?;
        attempt.result?;

        assert_eq!(
            std::fs::read_to_string(root.join("package.json"))?,
            authorized_manifest
        );
        let calls = command
            .calls
            .lock()
            .map_err(|_| eyre::eyre!("candidate command call log was poisoned"))?;
        assert_eq!(calls.len(), 3);
        let fallback_and_resync = calls
            .get(1..)
            .ok_or_else(|| eyre::eyre!("fallback commands were not recorded"))?;
        assert!(
            fallback_and_resync
                .iter()
                .flatten()
                .all(|arg| !arg.starts_with("--before="))
        );
        Ok(())
    }

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
    /// nothing is marked, so no ceiling applies. Same for no tag at all.
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
    fn window_minutes_from_cutoff_handles_spans_and_absolute_instants() {
        let now: jiff::Timestamp = "2024-08-15T00:00:00Z".parse().unwrap();
        // The application renders an age window as a relative span; each maps directly to minutes.
        assert_eq!(
            window_minutes_from_cutoff(Some("14 days"), now),
            Some(14 * 24 * 60)
        );
        assert_eq!(
            window_minutes_from_cutoff(Some("1 day"), now),
            Some(24 * 60)
        );
        assert_eq!(
            window_minutes_from_cutoff(Some("36 hours"), now),
            Some(36 * 60)
        );
        assert_eq!(window_minutes_from_cutoff(Some("1 hour"), now), Some(60));
        // Sub-minute ages round up so the cooldown is never silently disabled.
        assert_eq!(window_minutes_from_cutoff(Some("90 seconds"), now), Some(2));
        assert_eq!(window_minutes_from_cutoff(Some("30 seconds"), now), Some(1));
        // An absolute freeze instant converts to `now - instant` minutes (14 days here).
        assert_eq!(
            window_minutes_from_cutoff(Some("2024-08-01T00:00:00Z"), now),
            Some(14 * 24 * 60)
        );
        // A future instant (or no cutoff) excludes nothing → None.
        assert_eq!(
            window_minutes_from_cutoff(Some("2024-09-01T00:00:00Z"), now),
            None
        );
        assert_eq!(window_minutes_from_cutoff(None, now), None);
    }

    #[test]
    fn absolute_cutoff_from_project_realizes_relative_windows_for_npm() {
        let now: jiff::Timestamp = "2024-08-15T12:34:56Z".parse().unwrap();

        assert_eq!(
            absolute_cutoff_from_project(Some("14 days"), now).as_deref(),
            Some("2024-08-01T12:34:56Z")
        );
        assert_eq!(
            absolute_cutoff_from_project(Some("90 seconds"), now).as_deref(),
            Some("2024-08-15T12:33:26Z")
        );
        assert_eq!(
            absolute_cutoff_from_project(Some("2024-07-01T00:00:00Z"), now).as_deref(),
            Some("2024-07-01T00:00:00Z")
        );
        assert_eq!(
            absolute_cutoff_from_project(Some("0 days"), now).as_deref(),
            Some("2024-08-15T12:34:56Z")
        );
        assert_eq!(absolute_cutoff_from_project(None, now), None);
    }

    #[test]
    fn cutoff_fallback_removes_only_the_before_argument() {
        let args = vec![
            "install".to_string(),
            "eslint@10.6.0".to_string(),
            "--before=2026-06-30T00:00:00Z".to_string(),
            "--package-lock-only".to_string(),
        ];

        assert_eq!(
            without_before(&args),
            Some(vec![
                "install".to_string(),
                "eslint@10.6.0".to_string(),
                "--package-lock-only".to_string(),
            ])
        );
        assert_eq!(without_before(&["install".to_string()]), None);
    }

    #[test]
    fn whole_graph_args_pins_each_per_package_target_only_for_pnpm() {
        // pnpm pins each planned candidate to its EXACT per-package target in one joint resolve, so a
        // stricter-windowed package lands at its own (possibly older) target rather than overshooting.
        // The window rides inline as `minimumReleaseAge`, the floor for any fresh transitive the pins
        // drag in.
        // Each exact pin becomes `name@target`. Multi-version candidates stay out of this command
        // before construction because bare `pnpm update <name>` can write an out-of-range lock entry
        // while `--no-save` leaves the manifest unchanged. Importer filters cover both root and member
        // declarations without running the command in unrelated workspace packages.
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
    fn configured_string_list_accepts_pnpm_singletons_and_arrays() {
        let one = serde_json::from_str::<ConfigStringList>("\"nanoid\"")
            .expect("singleton config value parses");
        let many = serde_json::from_str::<ConfigStringList>("[\"nanoid\", \"eslint\"]")
            .expect("array config value parses");

        assert_eq!(one.into_vec(), vec!["nanoid".to_string()]);
        assert_eq!(
            many.into_vec(),
            vec!["nanoid".to_string(), "eslint".to_string()]
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
        // A multi-version dependency: `pkgs/low` is on the v22 line, `pkgs/high` on v25. A candidate
        // bumping `pkgs/low` to 25 must be judged at `pkgs/low`'s own copy (still 22) — NOT the name's
        // newest copy (25, owned by `pkgs/high`), which would falsely report it landed.
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
        // baseline. That net move must surface as a collateral row beside the held skip instead of
        // being silently dropped behind the planned name.
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

    #[test]
    fn peer_conflict_blocker_names_a_unique_peer_suffixed_sibling() {
        // `pkg-b` carries a `(shared@1.4.0)` peer suffix — its identity depends on the peer choice the
        // resolver made, which excluded the held `pkg-a`. With a single such sibling, blame is
        // unambiguous and `pkg-b` is named.
        let lock = "lockfileVersion: '9.0'\n\npackages:\n\n  pkg-a@1.0.0:\n    resolution: {integrity: sha512-a}\n\n  pkg-b@2.0.0(shared@1.4.0):\n    resolution: {integrity: sha512-b}\n\n  shared@1.4.0:\n    resolution: {integrity: sha512-c}\n";
        assert_eq!(
            peer_conflict_blocker(lock, "pkg-a"),
            Some("pkg-b".to_string())
        );
        // The held package's own peer-suffixed key never blames itself.
        let self_only = "lockfileVersion: '9.0'\n\npackages:\n\n  pkg-a@1.0.0(shared@2.0.0):\n    resolution: {integrity: sha512-a}\n";
        assert_eq!(peer_conflict_blocker(self_only, "pkg-a"), None);
    }

    #[test]
    fn peer_conflict_blocker_is_generic_when_blame_is_ambiguous() {
        // Two distinct peer-suffixed siblings make blame ambiguous → None (generic message).
        let lock = "lockfileVersion: '9.0'\n\npackages:\n\n  pkg-b@2.0.0(shared@1.0.0):\n    resolution: {integrity: sha512-b}\n\n  pkg-c@2.0.0(shared@1.0.0):\n    resolution: {integrity: sha512-c}\n";
        assert_eq!(peer_conflict_blocker(lock, "pkg-a"), None);
    }

    #[test]
    fn set_yaml_scalar_adds_updates_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml")).expect("utf8 path");
        std::fs::write(&path, "packages:\n  - \"a\"\n# keep me\n").expect("write");

        // Absent key → prepended, comments and existing content preserved.
        assert!(set_yaml_scalar(&path, "minimumReleaseAge", "20160", false).expect("set"));
        let after = std::fs::read_to_string(&path).expect("read");
        assert!(after.contains("minimumReleaseAge: 20160"));
        assert!(after.contains("# keep me"), "comments preserved");
        assert!(after.contains("packages:"), "existing content preserved");

        // Idempotent.
        assert!(!set_yaml_scalar(&path, "minimumReleaseAge", "20160", false).expect("again"));

        // Update in place.
        assert!(set_yaml_scalar(&path, "minimumReleaseAge", "30", false).expect("update"));
        let updated = std::fs::read_to_string(&path).expect("read");
        assert!(updated.contains("minimumReleaseAge: 30"));
        assert!(!updated.contains("20160"));
    }

    #[test]
    fn set_yaml_scalar_dry_run_reports_change_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml")).expect("utf8 path");
        let before = "packages:\n  - \"a\"\n";
        std::fs::write(&path, before).expect("write");

        // Dry run on an absent key reports it would change but writes nothing.
        assert!(set_yaml_scalar(&path, "minimumReleaseAge", "20160", true).expect("dry add"));
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);

        // Dry run on a missing file reports a change but does not create the file.
        let missing =
            Utf8PathBuf::from_path_buf(dir.path().join("absent.yaml")).expect("utf8 path");
        assert!(set_yaml_scalar(&missing, "minimumReleaseAge", "20160", true).expect("dry new"));
        assert!(!missing.exists(), "dry run must not create the file");
    }

    #[test]
    fn set_yaml_block_list_adds_updates_removes_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml")).expect("utf8 path");
        std::fs::write(
            &path,
            "minimumReleaseAge: 20160\npackages:\n  - \"a\"\n# keep me\n",
        )
        .expect("write");

        // Absent key → block appended, the rest of the document (scalar, packages, comment) preserved.
        let items = vec![
            "@typescript/native-preview".to_string(),
            "@scope/*".to_string(),
        ];
        assert!(
            set_yaml_block_list(&path, "minimumReleaseAgeExclude", &items, false).expect("add")
        );
        let after = std::fs::read_to_string(&path).expect("read");
        assert!(after.contains(
            "minimumReleaseAgeExclude:\n  - \"@typescript/native-preview\"\n  - \"@scope/*\""
        ));
        assert!(
            after.contains("minimumReleaseAge: 20160"),
            "scalar preserved"
        );
        assert!(after.contains("packages:"), "packages preserved");
        assert!(after.contains("# keep me"), "comment preserved");

        // Idempotent: the same items rewrite nothing.
        assert!(
            !set_yaml_block_list(&path, "minimumReleaseAgeExclude", &items, false).expect("again")
        );

        // Update in place: a different list replaces the block.
        let fewer = vec!["@typescript/native-preview".to_string()];
        assert!(
            set_yaml_block_list(&path, "minimumReleaseAgeExclude", &fewer, false).expect("update")
        );
        let updated = std::fs::read_to_string(&path).expect("read");
        assert!(
            updated.contains("minimumReleaseAgeExclude:\n  - \"@typescript/native-preview\"\n")
        );
        assert!(!updated.contains("@scope/*"), "dropped item is gone");
        assert!(updated.contains("# keep me"), "comment still preserved");

        // Empty list → the key and its block are removed entirely.
        assert!(
            set_yaml_block_list(&path, "minimumReleaseAgeExclude", &[], false).expect("remove")
        );
        let removed = std::fs::read_to_string(&path).expect("read");
        assert!(!removed.contains("minimumReleaseAgeExclude"), "key removed");
        assert!(
            removed.contains("minimumReleaseAge: 20160"),
            "scalar untouched"
        );
        // Removing again is a no-op.
        assert!(!set_yaml_block_list(&path, "minimumReleaseAgeExclude", &[], false).expect("noop"));
    }

    #[test]
    fn set_yaml_string_map_replaces_only_the_requested_block() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml")).expect("utf8 path");
        std::fs::write(
            &path,
            "minimumReleaseAge: 20160\noverrides:\n  existing: \"1.0.0\"\npackages:\n  - \"a\"\n# keep me\n",
        )
        .expect("write");
        let items = BTreeMap::from([
            ("@scope/pkg".to_string(), "2.0.0".to_string()),
            ("existing".to_string(), "1.1.0".to_string()),
        ]);

        assert!(set_yaml_string_map(&path, "overrides", &items).expect("replace"));
        let written = std::fs::read_to_string(&path).expect("read");
        assert!(
            written
                .contains("overrides:\n  \"@scope/pkg\": \"2.0.0\"\n  \"existing\": \"1.1.0\"\n")
        );
        assert!(written.contains("minimumReleaseAge: 20160"));
        assert!(written.contains("packages:\n  - \"a\""));
        assert!(written.contains("# keep me"));
        assert!(!set_yaml_string_map(&path, "overrides", &items).expect("idempotent"));
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
    async fn mutation_journal_restores_manifest_and_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(root.join("package.json"), "{\"name\":\"demo\"}").expect("manifest");
        std::fs::write(root.join("package-lock.json"), "{\"original\":true}").expect("lock");
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
            .await
            .expect("journal");
        std::fs::write(root.join("package.json"), "{\"mutated\":true}").expect("mutate manifest");
        std::fs::write(root.join("package-lock.json"), "{\"mutated\":true}").expect("mutate lock");
        captured.restore(&project.root).expect("restore");

        let restored_manifest =
            std::fs::read_to_string(root.join("package.json")).expect("read manifest");
        assert_eq!(restored_manifest, "{\"name\":\"demo\"}");
        let restored = std::fs::read_to_string(root.join("package-lock.json")).expect("read lock");
        assert_eq!(restored, "{\"original\":true}");
    }

    #[tokio::test]
    async fn mutation_journal_restores_member_manifests() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("apps/a")).expect("mkdir");
        std::fs::write(root.join("package.json"), "{\"name\":\"root\"}").expect("root manifest");
        std::fs::write(
            root.join("apps/a/package.json"),
            r#"{ "dependencies": { "nanoid": "^3.0.0" } }"#,
        )
        .expect("member manifest");
        std::fs::write(root.join("package-lock.json"), "{\"original\":true}").expect("lock");
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
            .await
            .expect("journal");
        std::fs::write(root.join("apps/a/package.json"), "{\"mutated\":true}")
            .expect("mutate member");
        captured.restore(&project.root).expect("restore");

        let restored =
            std::fs::read_to_string(root.join("apps/a/package.json")).expect("read member");
        assert_eq!(restored, r#"{ "dependencies": { "nanoid": "^3.0.0" } }"#);
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

    /// The fumadocs shape as a pnpm lock: the root importer declares `fumadocs-core` and
    /// `fumadocs-mdx`; mdx peer-requires `fumadocs-core@^16.0.0`.
    const PEER_LOCK: &str = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              fumadocs-core:
                specifier: ^16.0.0
                version: 16.11.4
              fumadocs-mdx:
                specifier: ^15.0.0
                version: 15.1.1(fumadocs-core@16.11.4)

        packages:

          fumadocs-core@16.11.4:
            resolution: {integrity: sha512-aaa}

          fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
            resolution: {integrity: sha512-bbb}
            peerDependencies:
              fumadocs-core: ^16.0.0
    "};

    fn plan_of(changes: Vec<Change>) -> Plan {
        Plan {
            changes,
            ..Plan::default()
        }
    }

    /// Gathers lock-only peer evidence (no workspace root, so no manifest source) and partitions —
    /// the shape most gate tests exercise.
    fn peer_partition<L: NodeLock>(plan: &Plan, lock: Option<&str>) -> PeerPartition {
        partition_peer_held::<L>(plan, &PeerEvidence::gather::<L>(None, lock))
    }

    /// The trap itself: a cross-major target a still-present dependent's peer range excludes is
    /// held up front, naming the dependent and its verbatim range — pnpm would only warn and land
    /// the break silently.
    #[test]
    fn peer_gate_holds_a_cross_major_target_excluded_by_a_dependent_range() {
        let plan = plan_of(vec![change("fumadocs-core", "16.11.4", "17.0.0")]);

        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan, Some(PEER_LOCK));

        assert!(
            retained.changes.is_empty(),
            "the gated change never resolves"
        );
        let held = skipped.first().expect("one peer hold");
        assert_eq!(held.reason, SkipReason::PeerHeld);
        assert_eq!(
            held.offending.as_ref().map(|package| package.name.as_str()),
            Some("fumadocs-mdx")
        );
        assert_eq!(
            held.detail.as_deref(),
            Some("held: fumadocs-mdx@15.1.1 requires fumadocs-core@^16.0.0")
        );
    }

    /// A peer range that unions majors (`^7.0.0 || ^8.0.0`, the common peer idiom) gates a move
    /// beyond the union and passes one within it.
    #[test]
    fn peer_gate_judges_union_ranges() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  eslint:
                    specifier: ^8.40.0
                    version: 8.57.0
                  '@typescript-eslint/eslint-plugin':
                    specifier: 6.21.0
                    version: 6.21.0(eslint@8.57.0)

            packages:

              '@typescript-eslint/eslint-plugin@6.21.0':
                resolution: {integrity: sha512-aaa}
                peerDependencies:
                  eslint: ^7.0.0 || ^8.0.0
        "};

        // 8 → 9 leaves the union: held, blaming the plugin.
        let cross = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&cross, Some(lock));
        assert!(retained.changes.is_empty());
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("@typescript-eslint/eslint-plugin")
        );

        // 7 → 8 stays within the union: the resolver's business.
        let within = plan_of(vec![change("eslint", "7.32.0", "8.57.0")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&within, Some(lock));
        assert_eq!(retained.changes.len(), 1);
        assert!(skipped.is_empty());
    }

    /// A *transitive* dependent never gates: the resolver may float it within its parents' ranges
    /// to a sibling version whose peer range admits the target (npm does exactly this), so its
    /// lock-recorded peer range is not authoritative — the real-world `eslint-plugin-jsdoc` shape.
    #[test]
    fn peer_gate_never_gates_on_a_transitive_dependent() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  eslint:
                    specifier: ^9.16.0
                    version: 9.16.0
                  eslint-config-treesitter:
                    specifier: ^1.0.2
                    version: 1.0.2(eslint@9.16.0)

            packages:

              eslint-plugin-jsdoc@50.6.0(eslint@9.16.0):
                resolution: {integrity: sha512-aaa}
                peerDependencies:
                  eslint: ^7.0.0 || ^8.0.0 || ^9.0.0
        "};

        let plan = plan_of(vec![change("eslint", "9.16.0", "10.6.0")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
        assert_eq!(
            retained.changes.len(),
            1,
            "a transitive dependent's peer range must not hold the move"
        );
        assert!(skipped.is_empty());
    }

    /// Fail-open rules: an in-range move is the resolver's business, and a dependent moving in the
    /// same plan may lift its own peer range, so joint moves stay with the resolver too.
    #[test]
    fn peer_gate_passes_in_range_moves_and_joint_moves() {
        // A minor move inside the peer range never gates.
        let minor = plan_of(vec![change("fumadocs-core", "16.11.4", "16.13.0")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&minor, Some(PEER_LOCK));
        assert_eq!(retained.changes.len(), 1);
        assert!(skipped.is_empty());

        // The dependent co-moves in the same plan: the resolver decides joint feasibility. This is
        // deliberately fail-open — the lock records only the dependent's *current* peer range, so
        // whether its target admits the moved package is unknowable here; the resolve that follows
        // is the authority (pnpm settles both peer contexts in its one whole-graph pass).
        let joint = plan_of(vec![
            change("fumadocs-core", "16.11.4", "17.0.0"),
            change("fumadocs-mdx", "15.1.1", "16.0.0"),
        ]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&joint, Some(PEER_LOCK));
        assert_eq!(retained.changes.len(), 2);
        assert!(skipped.is_empty());

        // No lock captured (a fresh project): nothing to prove, nothing gated.
        let plan = plan_of(vec![change("fumadocs-core", "16.11.4", "17.0.0")]);
        let PeerPartition { retained, skipped } = peer_partition::<crate::lock::Pnpm>(&plan, None);
        assert_eq!(retained.changes.len(), 1);
        assert!(skipped.is_empty());
    }

    /// A dependent declared only by *other* importers keeps its own in-range copy of the package
    /// (pnpm resolves peers per importing context), so a change scoped to disjoint members passes.
    #[test]
    fn peer_gate_passes_a_change_whose_members_are_disjoint_from_the_dependent() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/site:
                dependencies:
                  fumadocs-core:
                    specifier: ^16.0.0
                    version: 16.11.4

              apps/docs:
                dependencies:
                  fumadocs-core:
                    specifier: ^16.0.0
                    version: 16.11.4
                  fumadocs-mdx:
                    specifier: ^15.0.0
                    version: 15.1.1(fumadocs-core@16.11.4)

            packages:

              fumadocs-core@16.11.4:
                resolution: {integrity: sha512-aaa}

              fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
                resolution: {integrity: sha512-bbb}
                peerDependencies:
                  fumadocs-core: ^16.0.0
        "};
        let member = |path: &str| MemberRef {
            name: path.to_string(),
            path: path.to_string(),
        };

        // fumadocs-core is declared by both importers, so it is multi-version-safe here only via
        // members: a change scoped to `apps/site` cannot break `apps/docs`'s mdx peer.
        let mut site = change("fumadocs-core", "16.11.4", "17.0.0");
        site.members = vec![member("apps/site")];
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan_of(vec![site]), Some(lock));
        assert_eq!(retained.changes.len(), 1, "disjoint importers pass");
        assert!(skipped.is_empty());

        // Scoped to the importer that also declares the dependent, the gate fires.
        let mut docs = change("fumadocs-core", "16.11.4", "17.0.0");
        docs.members = vec![member("apps/docs")];
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan_of(vec![docs]), Some(lock));
        assert!(retained.changes.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    /// Exclusion must be *proven*: a range with a branch the matcher cannot represent (an npm
    /// hyphen range) yields `Unknown`, never a hold — while a fully understood union (x-wildcards
    /// included) still gates.
    #[test]
    fn peer_gate_never_holds_on_a_range_with_an_unrepresentable_branch() {
        let lock_with_range = |range: &str| {
            formatdoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      fumadocs-core:
                        specifier: ^16.0.0
                        version: 16.11.4
                      fumadocs-mdx:
                        specifier: ^15.0.0
                        version: 15.1.1(fumadocs-core@16.11.4)

                packages:

                  fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
                    resolution: {{integrity: sha512-bbb}}
                    peerDependencies:
                      fumadocs-core: '{range}'
            "}
        };
        let plan = plan_of(vec![change("fumadocs-core", "16.11.4", "18.0.0")]);

        // The hyphen branch is unrepresentable: current matches `^16.0.0`, but excluding 18.0.0
        // cannot be proven, so the move passes to the resolver.
        let union = lock_with_range("^16.0.0 || 17.0.0 - 17.4.0");
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan, Some(&union));
        assert_eq!(retained.changes.len(), 1, "unproven exclusion never holds");
        assert!(skipped.is_empty());

        // The x-wildcard union is fully understood: 18.0.0 is provably outside it.
        let wildcard = lock_with_range("^16.0.0 || 17.x");
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan, Some(&wildcard));
        assert!(retained.changes.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    /// An *optional* peer gates like any other when the peer is present: optionality tolerates
    /// absence (npm skips auto-installing it), not a present copy outside the declared range — and
    /// the queried peer is by construction present, it is the package being upgraded.
    #[test]
    fn peer_gate_holds_an_optional_peer_that_is_present_but_incompatible() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  typescript:
                    specifier: ^5.5.0
                    version: 5.5.4
                  ts-linter:
                    specifier: ^3.0.0
                    version: 3.2.0(typescript@5.5.4)

            packages:

              ts-linter@3.2.0(typescript@5.5.4):
                resolution: {integrity: sha512-aaa}
                peerDependencies:
                  typescript: '>=5 <6'
                peerDependenciesMeta:
                  typescript:
                    optional: true
        "};

        let plan = plan_of(vec![change("typescript", "5.5.4", "6.0.0")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
        assert!(retained.changes.is_empty());
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("ts-linter")
        );
    }

    /// `0.1 → 0.2` crosses an npm compatibility line (caret semantics make the 0.x minor the
    /// breaking axis), so it is gated exactly like a numeric major jump — while a same-line `0.1`
    /// patch move stays the resolver's business.
    #[test]
    fn peer_gate_gates_a_zero_line_jump() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  zod-mini:
                    specifier: ~0.1.0
                    version: 0.1.5
                  zod-adapter:
                    specifier: ^2.0.0
                    version: 2.0.0(zod-mini@0.1.5)

            packages:

              zod-adapter@2.0.0(zod-mini@0.1.5):
                resolution: {integrity: sha512-aaa}
                peerDependencies:
                  zod-mini: ~0.1.0
        "};

        let cross = plan_of(vec![change("zod-mini", "0.1.5", "0.2.0")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&cross, Some(lock));
        assert!(retained.changes.is_empty(), "0.1 → 0.2 is a breaking jump");
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("zod-adapter")
        );

        let within = plan_of(vec![change("zod-mini", "0.1.5", "0.1.9")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&within, Some(lock));
        assert_eq!(retained.changes.len(), 1);
        assert!(skipped.is_empty());
    }

    /// In `0.0.x` the caret admits nothing beyond the exact version (`^0.0.3` ⇔ `=0.0.3`), so even
    /// a patch step is a breaking move: a dependent's `^0.0.3` provably excludes `0.0.4` and the
    /// gate must consult that proof rather than exit on "same line".
    #[test]
    fn peer_gate_gates_a_double_zero_patch_step() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  proto-kit:
                    specifier: ^0.0.3
                    version: 0.0.3
                  proto-kit-adapter:
                    specifier: ^1.0.0
                    version: 1.0.0(proto-kit@0.0.3)

            packages:

              proto-kit-adapter@1.0.0(proto-kit@0.0.3):
                resolution: {integrity: sha512-aaa}
                peerDependencies:
                  proto-kit: ^0.0.3
        "};

        let plan = plan_of(vec![change("proto-kit", "0.0.3", "0.0.4")]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
        assert!(
            retained.changes.is_empty(),
            "a 0.0.x step that provably breaks a peer range must hold"
        );
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("proto-kit-adapter")
        );
    }

    /// npm's package-lock attributes importer declarations by *name* only, but the physical
    /// layout is instance-exact: the member's own nearest-ancestor lookup identifies its direct
    /// copy, so a name resolved at several versions is no longer ambiguity. Both directions
    /// matter — the nested copy's stricter range must not be promoted to a blocker, and the
    /// direct copy's stricter range must not be blinded by the nested split (the escape a blanket
    /// split fail-open used to leave).
    #[test]
    fn peer_gate_resolves_npm_name_splits_physically() {
        let nested_would_block = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "fixture",
                    "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^2.0.0" }
                },
                "node_modules/eslint": { "version": "8.57.0" },
                "node_modules/eslint-plugin-legacy": {
                    "version": "2.0.0",
                    "peerDependencies": { "eslint": "^8.0.0 || ^9.0.0" }
                },
                "node_modules/report-tool/node_modules/eslint-plugin-legacy": {
                    "version": "1.0.0",
                    "peerDependencies": { "eslint": "^8.0.0" }
                }
            }
        }"#};
        let plan = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);

        // The nested 1.0.0 copy would bind, but the root's lookup proves its direct copy is the
        // admitting 2.0.0 — the nested record is the transitive one and holds nothing.
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Npm>(&plan, Some(nested_would_block));
        assert_eq!(
            retained.changes.len(),
            1,
            "the nested transitive copy must not be promoted to a blocker"
        );
        assert!(skipped.is_empty());

        // The inverse split: the DIRECT copy blocks while a nested copy admits. The split must
        // not blind the gate — the root's own instance is identified physically and holds.
        let direct_blocks = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "fixture",
                    "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^1.0.0" }
                },
                "node_modules/eslint": { "version": "8.57.0" },
                "node_modules/eslint-plugin-legacy": {
                    "version": "1.0.0",
                    "peerDependencies": { "eslint": "^8.0.0" }
                },
                "node_modules/report-tool/node_modules/eslint-plugin-legacy": {
                    "version": "2.0.0",
                    "peerDependencies": { "eslint": "^8.0.0 || ^9.0.0" }
                }
            }
        }"#};
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Npm>(&plan, Some(direct_blocks));
        assert!(
            retained.changes.is_empty(),
            "the direct copy's contract holds despite the nested split"
        );
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("eslint-plugin-legacy")
        );

        // One resolved version + a declaring importer: that instance IS the direct dependency.
        let unambiguous = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "fixture",
                    "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^1.0.0" }
                },
                "node_modules/eslint": { "version": "8.57.0" },
                "node_modules/eslint-plugin-legacy": {
                    "version": "1.0.0",
                    "peerDependencies": { "eslint": "^8.0.0" }
                }
            }
        }"#};
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Npm>(&plan, Some(unambiguous));
        assert!(retained.changes.is_empty());
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("eslint-plugin-legacy")
        );
    }

    /// A published peer contract is never rewritten out from under itself, so a narrow
    /// author-written bound holds even a same-line move: `peerDependencies.chalk =
    /// ">=5.6.0 <5.6.2"` excludes a 5.6.0 → 5.6.2 patch bump that no compatibility-line test
    /// sees. The alternative — letting the widen shift the contract to `^5.6.2` so the move can
    /// land — silently drops the consumers on 5.6.0/5.6.1 that the author still supports.
    #[test]
    fn workspace_manifest_peer_holds_a_same_line_move_its_narrow_bound_excludes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = indoc! {r#"{
            "name": "root-lib",
            "version": "1.2.0",
            "devDependencies": { "chalk": "^5.6.0" },
            "peerDependencies": { "chalk": ">=5.6.0 <5.6.2" }
        }"#};
        std::fs::write(root.join("package.json"), manifest).expect("root manifest");
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "root-lib",
                    "version": "1.2.0",
                    "devDependencies": { "chalk": "^5.6.0" },
                    "peerDependencies": { "chalk": ">=5.6.0 <5.6.2" }
                },
                "node_modules/chalk": { "version": "5.6.0" }
            }
        }"#};

        let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(lock));
        let plan = plan_of(vec![change("chalk", "5.6.0", "5.6.2")]);
        let PeerPartition { retained, skipped } =
            partition_peer_held::<crate::lock::Npm>(&plan, &evidence);
        assert!(
            retained.changes.is_empty(),
            "a provable break holds even without crossing a compatibility line"
        );
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("root-lib"),
            "the hold names the local package whose contract must be edited"
        );

        // The gate holds the move *before* any widen runs, and a widen could not touch the
        // contract anyway: the published field is outside the write set, so the pre-apply
        // snapshot the post-resolve verifier judges against can never go stale.
        manifest::widen_constraints(&root, &[], "chalk", "5.6.2", RewriteMode::Always)
            .expect("widen");
        let after = std::fs::read_to_string(root.join("package.json")).expect("read manifest");
        assert!(
            after.contains(r#""chalk": ">=5.6.0 <5.6.2""#),
            "the published peer contract must survive a widen verbatim: {after}"
        );
        assert!(
            after.contains(r#""chalk": "^5.6.2""#),
            "the install declaration is still widened: {after}"
        );
    }

    /// A `fix` downgrade is exempt from the pre-gate (rolling back is its whole purpose), so a
    /// downgrade that lands below a local package's published peer floor is caught by the
    /// post-resolve verifier instead — and blamed on the moving package, since the local dependent
    /// never moves. Cooldown neither commits the break nor lowers the author's published floor: it
    /// reports and rolls back, leaving the remedy (relax the range, or baseline the violation) to
    /// the author.
    #[test]
    fn workspace_manifest_peer_floor_rejects_a_downgrade_below_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            indoc! {r#"{
                "name": "root-lib",
                "version": "1.2.0",
                "devDependencies": { "chalk": "^5.6.2" },
                "peerDependencies": { "chalk": ">=5.6.2" }
            }"#},
        )
        .expect("root manifest");
        let lock = |version: &str| {
            formatdoc! {r#"{{
                "lockfileVersion": 3,
                "packages": {{
                    "": {{
                        "name": "root-lib",
                        "version": "1.2.0",
                        "devDependencies": {{ "chalk": "^5.6.2" }},
                        "peerDependencies": {{ "chalk": ">=5.6.2" }}
                    }},
                    "node_modules/chalk": {{ "version": "{version}" }}
                }}
            }}"#}
        };
        let before = lock("5.6.2");
        let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(&before));

        // The pre-gate lets the downgrade through — `fix` must be able to roll back.
        let mut downgrade = change("chalk", "5.6.2", "5.6.0");
        downgrade.downgrade = true;
        let PeerPartition { retained, .. } =
            partition_peer_held::<crate::lock::Npm>(&plan_of(vec![downgrade.clone()]), &evidence);
        assert_eq!(retained.changes.len(), 1, "a downgrade is not pre-held");

        // The landed graph is then post-verified and rejected, blaming the moved package.
        let after = lock("5.6.0");
        let baseline = PeerBaseline::gather::<crate::lock::Npm>(Some(&before), &evidence.workspace);
        let current = proven_peer_violations::<crate::lock::Npm>(&after, &evidence.workspace);
        assert_eq!(
            current
                .keys()
                .map(|id| (id.dependent.as_str(), id.range.as_str()))
                .collect::<Vec<_>>(),
            vec![("root-lib", ">=5.6.2")],
            "the downgrade provably breaks the published floor"
        );
        let rejections = plan_peer_rejections(
            &baseline,
            &current,
            &plan_of(vec![downgrade]),
            &HashSet::new(),
        )
        .expect("uniquely attributable");
        assert_eq!(
            rejections
                .first()
                .map(|rejection| rejection.offending.as_str()),
            Some("root-lib"),
            "the rejection names the contract holder, not a guess"
        );
    }

    /// Workspace-manifest contracts survive the pre-gate into post-resolve verification: a
    /// *collateral* move — one the resolve dragged in, never a planned candidate — that breaks a
    /// local package's recorded contract is a proven violation with no culpable candidate, so the
    /// round escalates to candidate isolation instead of committing the break.
    #[test]
    fn workspace_manifest_peer_violations_are_post_verified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
        )
        .expect("root manifest");
        std::fs::write(
            root.join("packages/shim/package.json"),
            r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
        )
        .expect("member manifest");
        let before = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  eslint:
                    specifier: ^8.40.0
                    version: 8.57.0
                  local-eslint-shim:
                    specifier: workspace:*
                    version: link:packages/shim

              packages/shim: {}
        "};
        let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(before));
        assert!(
            proven_peer_violations::<crate::lock::Pnpm>(before, &evidence.workspace).is_empty(),
            "the pre-apply graph satisfies the shim's contract"
        );

        // The resolve floated eslint across the major on its own — no planned candidate did it.
        let after = before.replace("version: 8.57.0", "version: 9.8.0");
        let baseline = PeerBaseline::gather::<crate::lock::Pnpm>(Some(before), &evidence.workspace);
        let current = proven_peer_violations::<crate::lock::Pnpm>(&after, &evidence.workspace);
        assert_eq!(
            current
                .keys()
                .map(|id| id.dependent.as_str())
                .collect::<Vec<_>>(),
            vec!["local-eslint-shim"],
            "the workspace contract is checked after the resolve, not only before it"
        );
        assert!(
            plan_peer_rejections(
                &baseline,
                &current,
                &plan_of(vec![change("unrelated", "1.0.0", "1.1.0")]),
                &HashSet::new(),
            )
            .is_err(),
            "a collateral break with no culpable candidate escalates to candidate isolation"
        );
    }

    /// npm's package-lock deliberately records no peers for the root project, delegating them to
    /// the workspace-manifest source — and the root is not an installed instance either, so a
    /// name-keyed instance lookup finds nothing and the contract goes ungated. A root library
    /// declaring `peerDependencies.eslint = "^8"` must hold an eslint 8→9 move: without the hold,
    /// apply's manifest widening rewrites that very `peerDependencies` entry, silently changing
    /// the package's own published contract.
    #[test]
    fn workspace_manifest_peer_holds_the_root_projects_own_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            indoc! {r#"{
                "name": "root-lib",
                "version": "1.2.0",
                "devDependencies": { "eslint": "^8.40.0" },
                "peerDependencies": { "eslint": "^8.0.0" }
            }"#},
        )
        .expect("root manifest");
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "root-lib",
                    "version": "1.2.0",
                    "devDependencies": { "eslint": "^8.40.0" },
                    "peerDependencies": { "eslint": "^8.0.0" }
                },
                "node_modules/eslint": { "version": "8.57.0" }
            }
        }"#};

        let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(lock));
        let plan = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);
        let PeerPartition { retained, skipped } =
            partition_peer_held::<crate::lock::Npm>(&plan, &evidence);
        assert!(
            retained.changes.is_empty(),
            "the root project's own peer contract must gate the move"
        );
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("root-lib"),
            "the hold names the local package whose contract must be edited deliberately"
        );

        // The same contract, post-verified: a resolver move that lands eslint 9 anyway is a
        // proven violation of the root's recorded contract, so the apply can roll it back.
        let after = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "root-lib",
                    "version": "1.2.0",
                    "devDependencies": { "eslint": "^8.40.0" },
                    "peerDependencies": { "eslint": "^8.0.0" }
                },
                "node_modules/eslint": { "version": "9.8.0" }
            }
        }"#};
        assert!(
            proven_peer_violations::<crate::lock::Npm>(lock, &evidence.workspace).is_empty(),
            "the pre-apply graph satisfies the contract"
        );
        let violations = proven_peer_violations::<crate::lock::Npm>(after, &evidence.workspace);
        assert_eq!(
            violations
                .keys()
                .map(|id| (id.dependent.as_str(), id.package.as_str()))
                .collect::<Vec<_>>(),
            vec![("root-lib", "eslint")],
            "the landed graph provably breaks the root's contract"
        );
    }

    /// A workspace contract binds through its manifest's OWN directory, never through a same-named
    /// package elsewhere in the tree: a registry dependency that happens to share the local
    /// package's name must not stand in for it and fabricate a hold.
    #[test]
    fn workspace_manifest_peer_ignores_a_same_name_registry_decoy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
        std::fs::create_dir_all(root.join("apps/site")).expect("mkdir");
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root-app", "workspaces": ["apps/*", "packages/*"] }"#,
        )
        .expect("root manifest");
        std::fs::write(
            root.join("apps/site/package.json"),
            r#"{ "name": "site", "dependencies": { "eslint": "^8.40.0", "toolkit": "^1.0.0" } }"#,
        )
        .expect("app manifest");
        // The local package declares the peer, but its own directory holds a nested eslint copy
        // the change never rewrites. A registry `toolkit` of the same name sits hoisted at the
        // root and *does* resolve the rewritten copy — a name-keyed lookup would let it stand in
        // for the local package and fabricate a hold.
        std::fs::write(
            root.join("packages/shim/package.json"),
            r#"{ "name": "toolkit", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
        )
        .expect("member manifest");
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root-app" },
                "apps/site": {
                    "name": "site",
                    "dependencies": { "eslint": "^8.40.0", "toolkit": "^1.0.0" }
                },
                "packages/shim": { "name": "toolkit", "version": "0.1.0" },
                "packages/shim/node_modules/eslint": { "version": "8.57.0" },
                "node_modules/toolkit": { "version": "1.0.0" },
                "node_modules/eslint": { "version": "8.57.0" }
            }
        }"#};

        let evidence = PeerEvidence::gather::<crate::lock::Npm>(Some(&root), Some(lock));
        assert!(
            evidence
                .workspace
                .iter()
                .any(|peer| peer.origin == "packages/shim"),
            "the local package's contract is collected with its origin"
        );
        let mut eslint = change("eslint", "8.57.0", "9.8.0");
        eslint.members = vec![MemberRef {
            name: "site".to_string(),
            path: "apps/site".to_string(),
        }];
        let PeerPartition { retained, skipped } =
            partition_peer_held::<crate::lock::Npm>(&plan_of(vec![eslint]), &evidence);
        assert_eq!(
            retained.changes.len(),
            1,
            "the same-named registry copy must not bind the local contract: {skipped:?}"
        );
        assert!(
            proven_peer_violations::<crate::lock::Npm>(lock, &evidence.workspace).is_empty(),
            "the local package's own nested copy satisfies its contract"
        );
    }

    /// A *workspace-local* package's peer contract lives only in its own `package.json` — pnpm
    /// records a linked package in the lock without its peer metadata — so the gate must read the
    /// member manifests: a cross-major move that provably breaks a linked dependent's peer range
    /// is held, blamed on the local package. The binding contexts are the package's own directory
    /// plus its link consumers, so a change scoped to an unrelated importer still passes.
    #[test]
    fn workspace_manifest_peer_holds_a_cross_major_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
        )
        .expect("root manifest");
        std::fs::write(
            root.join("packages/shim/package.json"),
            r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
        )
        .expect("member manifest");
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  eslint:
                    specifier: ^8.40.0
                    version: 8.57.0
                  local-eslint-shim:
                    specifier: workspace:*
                    version: link:packages/shim

              packages/shim: {}
        "};

        let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(lock));
        assert_eq!(
            evidence.workspace.len(),
            1,
            "the shim's manifest peer is collected"
        );
        assert_eq!(
            evidence
                .workspace
                .first()
                .map(|peer| peer.contexts.clone())
                .unwrap_or_default(),
            vec![".".to_string(), "packages/shim".to_string()],
            "the peer binds in the shim's own dir and its link consumer"
        );

        // The provably breaking cross-major move is held, blamed on the local package.
        let plan = plan_of(vec![change("eslint", "8.57.0", "9.8.0")]);
        let PeerPartition { retained, skipped } =
            partition_peer_held::<crate::lock::Pnpm>(&plan, &evidence);
        assert!(retained.changes.is_empty());
        assert_eq!(
            skipped.first().and_then(|held| held.detail.as_deref()),
            Some("held: local-eslint-shim@0.1.0 requires eslint@^8.0.0")
        );

        // Scoped to an importer outside the peer's binding contexts, the same move passes.
        let mut scoped = change("eslint", "8.57.0", "9.8.0");
        scoped.members = vec![MemberRef {
            name: "other".into(),
            path: "apps/other".into(),
        }];
        let PeerPartition { retained, skipped } =
            partition_peer_held::<crate::lock::Pnpm>(&plan_of(vec![scoped]), &evidence);
        assert_eq!(retained.changes.len(), 1);
        assert!(skipped.is_empty());
    }

    /// An *injected* workspace dependency (`dependenciesMeta.*.injected`) is recorded as a
    /// root-relative `file:` version with a `(peer@x)` context suffix, not a `link:` — a second
    /// encoding of the same domain fact (a locally consumed package whose peers live in its
    /// manifest). The gate must reach the same hold through it: the injected shim's manifest peer
    /// range holds the cross-major move, blamed on the shim.
    #[test]
    fn workspace_manifest_peer_holds_an_injected_cross_major_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("packages/shim")).expect("mkdir");
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
        )
        .expect("root manifest");
        std::fs::write(
            root.join("packages/shim/package.json"),
            r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
        )
        .expect("member manifest");
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  eslint:
                    specifier: ^8.40.0
                    version: 8.57.1
                  local-eslint-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(eslint@8.57.1)'
                dependenciesMeta:
                  local-eslint-shim:
                    injected: true

              packages/shim: {}

            packages:

              local-eslint-shim@file:packages/shim:
                resolution: {directory: packages/shim, type: directory}
                peerDependencies:
                  eslint: ^8.0.0
        "};

        let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(lock));
        assert_eq!(
            evidence
                .workspace
                .first()
                .map(|peer| peer.contexts.clone())
                .unwrap_or_default(),
            vec![".".to_string(), "packages/shim".to_string()],
            "the peer binds in the shim's own dir and its injecting consumer"
        );

        let plan = plan_of(vec![change("eslint", "8.57.1", "10.8.0")]);
        let PeerPartition { retained, skipped } =
            partition_peer_held::<crate::lock::Pnpm>(&plan, &evidence);
        assert!(retained.changes.is_empty());
        assert_eq!(
            skipped.first().and_then(|held| held.detail.as_deref()),
            Some("held: local-eslint-shim@0.1.0 requires eslint@^8.0.0")
        );
    }

    /// The injected path itself may end in a parenthesized directory group carrying `@` —
    /// `file:packages/shim(foo@bar)(eslint@8.57.1)` is real pnpm 11 output for a member named
    /// `shim(foo@bar)` — so the gate must recover the path against the importer set and find the
    /// manifest there; any scalar-only suffix split would read the wrong directory (or none) and
    /// lose the hold.
    #[test]
    fn workspace_manifest_peer_holds_across_a_parenthesized_injected_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("packages/shim(foo@bar)")).expect("mkdir");
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root-app", "dependencies": { "eslint": "^8.40.0" } }"#,
        )
        .expect("root manifest");
        std::fs::write(
            root.join("packages/shim(foo@bar)/package.json"),
            r#"{ "name": "local-eslint-shim", "version": "0.1.0", "peerDependencies": { "eslint": "^8.0.0" } }"#,
        )
        .expect("member manifest");
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  eslint:
                    specifier: ^8.40.0
                    version: 8.57.1
                  local-eslint-shim:
                    specifier: workspace:*
                    version: 'file:packages/shim(foo@bar)(eslint@8.57.1)'

              'packages/shim(foo@bar)': {}
        "};

        let evidence = PeerEvidence::gather::<crate::lock::Pnpm>(Some(&root), Some(lock));
        let plan = plan_of(vec![change("eslint", "8.57.1", "10.8.0")]);
        let PeerPartition { retained, skipped } =
            partition_peer_held::<crate::lock::Pnpm>(&plan, &evidence);
        assert!(retained.changes.is_empty());
        assert_eq!(
            skipped.first().and_then(|held| held.detail.as_deref()),
            Some("held: local-eslint-shim@0.1.0 requires eslint@^8.0.0")
        );
    }

    /// npm's sequential per-package path never judges a pair jointly, so a co-moving dependent
    /// grants NO exemption there: the excluded target stays held against the dependent's current
    /// range while the dependent's own move proceeds (the next run reads its new range). pnpm's
    /// whole-graph resolve keeps the joint exemption — see
    /// `peer_gate_passes_in_range_moves_and_joint_moves`.
    #[test]
    fn peer_gate_grants_no_co_move_exemption_on_the_per_package_path() {
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "fixture",
                    "dependencies": { "eslint": "^8.40.0", "eslint-plugin-legacy": "^1.0.0" }
                },
                "node_modules/eslint": { "version": "8.57.0" },
                "node_modules/eslint-plugin-legacy": {
                    "version": "1.0.0",
                    "peerDependencies": { "eslint": "^8.0.0" }
                }
            }
        }"#};

        let joint = plan_of(vec![
            change("eslint", "8.57.0", "9.8.0"),
            change("eslint-plugin-legacy", "1.0.0", "2.0.0"),
        ]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Npm>(&joint, Some(lock));
        let retained_names: Vec<&str> = retained
            .changes
            .iter()
            .map(|change| change.package.name.as_str())
            .collect();
        assert_eq!(
            retained_names,
            vec!["eslint-plugin-legacy"],
            "the dependent's own move proceeds; the excluded target does not ride along"
        );
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("eslint-plugin-legacy")
        );
    }

    /// The sequential path's post-condition: a landed candidate whose lock now provably violates a
    /// peer contract the pre-candidate lock did not (the dependent moved alone and its *new* range
    /// excludes the still-held peer — what `legacy-peer-deps` commits with only a warning) is a
    /// break the candidate caused. A contract the graph already broke, or an unchanged lock, is
    /// never re-attributed to the candidate.
    #[test]
    fn post_apply_diff_detects_only_new_proven_peer_violations() {
        let before = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "fixture",
                    "dependencies": { "react": "^18.3.1", "react-dom": "^18.3.1" }
                },
                "node_modules/react": { "version": "18.3.1" },
                "node_modules/react-dom": {
                    "version": "18.3.1",
                    "peerDependencies": { "react": "^18.3.1" }
                }
            }
        }"#};
        let after = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "fixture",
                    "dependencies": { "react": "^18.3.1", "react-dom": "^19.1.0" }
                },
                "node_modules/react": { "version": "18.3.1" },
                "node_modules/react-dom": {
                    "version": "19.1.0",
                    "peerDependencies": { "react": "^19.1.0" }
                }
            }
        }"#};

        let violation = first_new_peer_violation::<crate::lock::Npm>(Some(before), after)
            .expect("the dependent's new range provably excludes the held peer");
        assert_eq!(
            (
                violation.dependent.as_str(),
                violation.dependent_version.as_str(),
                violation.package.as_str(),
                violation.range.as_str(),
            ),
            ("react-dom", "19.1.0", "react", "^19.1.0")
        );

        assert!(
            first_new_peer_violation::<crate::lock::Npm>(Some(before), before).is_none(),
            "an unchanged lock introduces nothing"
        );
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(Some(after), after).is_none(),
            "a pre-existing violation is not re-attributed to the candidate"
        );
    }

    /// The post-condition holds only on proof, the gate's shared rule: a dependent whose own
    /// context binds a *satisfying* nested copy, an absent peer (possibly optional), or a range
    /// the translator cannot prove all yield nothing.
    #[test]
    fn post_apply_diff_fails_open_without_proof() {
        let satisfied_nested = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "react-dom": "^19.1.0" } },
                "node_modules/react": { "version": "18.3.1" },
                "node_modules/react-dom": {
                    "version": "19.1.0",
                    "peerDependencies": { "react": "^19.1.0" }
                },
                "node_modules/react-dom/node_modules/react": { "version": "19.1.0" }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, satisfied_nested).is_none(),
            "the dependent's own lookup binds its satisfying nested copy, not the root's 18.x"
        );

        let absent_peer = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "react-dom": "^19.1.0" } },
                "node_modules/react-dom": {
                    "version": "19.1.0",
                    "peerDependencies": { "react": "^19.1.0" }
                }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, absent_peer).is_none(),
            "an absent peer may be legitimately optional"
        );

        let unprovable_range = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "react-dom": "^19.1.0" } },
                "node_modules/react": { "version": "18.3.1" },
                "node_modules/react-dom": {
                    "version": "19.1.0",
                    "peerDependencies": { "react": "next" }
                }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, unprovable_range).is_none(),
            "an unprovable range is ignorance, not proof of exclusion"
        );

        // eslint moved to 10 while a *transitive* plugin's (optional) peer range still names ^9 —
        // the shape every eslint plugin creates. The dependent is not importer-declared, so its
        // recorded range is not authoritative (the pre-apply gate's own directness rule) and the
        // resolver's acceptance stands.
        let transitive_dependent = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "eslint": "^10.0.0" } },
                "node_modules/eslint": { "version": "10.6.0" },
                "node_modules/eslint-plugin-jsdoc": {
                    "version": "50.6.1",
                    "peerDependencies": { "eslint": "^9.0.0" }
                }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, transitive_dependent).is_none(),
            "a transitive dependent's stale peer range must not veto the accepted move"
        );

        // A direct plugin whose violated peer is *transitive* — `@typescript-eslint/parser`,
        // present only as the plugin's auto-installed peer, lagging behind the plugin's new major.
        // The resolver owns a transitive peer's placement (it accepted this graph and can
        // re-place the copy per context), so its lag must not veto the direct move — only a
        // contract between two importer-declared packages gates.
        let transitive_peer = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "plugin": "^8.0.0" } },
                "node_modules/plugin": {
                    "version": "8.0.0",
                    "peerDependencies": { "parser": "^8.0.0" }
                },
                "node_modules/parser": { "version": "7.18.0" }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, transitive_peer).is_none(),
            "a lagging transitive peer must not veto the direct move the resolver accepted"
        );

        // The root's own lookup resolves plugin@1 — the physically proven direct copy — so the
        // nested plugin@2's peer range is a transitive contract that must not masquerade as a
        // direct one (the pre-gate's directness rule, shared via `direct_dependent_members`).
        let split_resolved_dependent = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "eslint": "^10.0.0", "plugin": "^1.0.0" } },
                "node_modules/eslint": { "version": "10.6.0" },
                "node_modules/plugin": { "version": "1.0.0" },
                "node_modules/other/node_modules/plugin": {
                    "version": "2.0.0",
                    "peerDependencies": { "eslint": "^9.0.0" }
                }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, split_resolved_dependent).is_none(),
            "a nested copy is not the direct instance any member's lookup resolves"
        );
    }

    /// Contextual binding replaces the global-singleton requirement: a peer resolved at several
    /// versions is judged per context — npm by the dependent instance's own lookup, pnpm by each
    /// importer's declared copy — so a genuine break against the bound copy is proven even while
    /// another version exists elsewhere in the graph.
    #[test]
    fn post_apply_diff_binds_peers_per_context() {
        // npm: the dependent's context binds the root host@1.0.0; an unrelated nested host@0.9.0
        // also exists. The graph-wide split must not suppress the proven break against the copy
        // the dependent actually sees.
        let npm_bound_break = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "plugin": "^3.0.0", "host": "^1.0.0" } },
                "node_modules/plugin": {
                    "version": "3.0.0",
                    "peerDependencies": { "host": "^2.0.0" }
                },
                "node_modules/host": { "version": "1.0.0" },
                "node_modules/report-tool/node_modules/host": { "version": "0.9.0" }
            }
        }"#};
        let violation = first_new_peer_violation::<crate::lock::Npm>(None, npm_bound_break)
            .expect("the bound root copy provably violates; the nested split must not blind it");
        assert_eq!(
            (violation.package.as_str(), violation.range.as_str()),
            ("host", "^2.0.0")
        );

        // pnpm: importers bind their own declared copies — apps/a's plugin sees apps/a's
        // host@1.0.0 (a proven break) even though apps/b resolves host@2.0.0.
        let pnpm_importer_break = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/a:
                dependencies:
                  plugin:
                    specifier: ^3.0.0
                    version: 3.0.0
                  host:
                    specifier: ^1.0.0
                    version: 1.0.0

              apps/b:
                dependencies:
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

            packages:

              plugin@3.0.0:
                resolution: {integrity: sha512-p3}
                peerDependencies:
                  host: ^2.0.0

              host@1.0.0:
                resolution: {integrity: sha512-h1}

              host@2.0.0:
                resolution: {integrity: sha512-h2}
        "};
        let violation = first_new_peer_violation::<crate::lock::Pnpm>(None, pnpm_importer_break)
            .expect(
                "apps/a's own declared copy provably violates despite the cross-importer split",
            );
        assert_eq!(violation.dependent.as_str(), "plugin");
    }

    /// Peers resolve against the dependent's importing context — and the context is defined by
    /// the layout the manager materializes. pnpm isolates importers by declaration, so a package
    /// moved in a *disjoint* importer cannot break a dependent that never sees it. npm's default
    /// layout *hoists*: the same disjoint declarations meet at the root `node_modules`, so there
    /// the contract genuinely binds — and only a physically shadowed dependent (its own nested
    /// copy) stays out of reach.
    #[test]
    fn post_apply_diff_requires_importer_context_overlap() {
        let split_pnpm_importers = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/a:
                dependencies:
                  plugin:
                    specifier: ^1.0.0
                    version: 1.0.0

              apps/b:
                dependencies:
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

            packages:

              plugin@1.0.0:
                peerDependencies:
                  host: ^1.0.0

              host@2.0.0:
                resolution: {integrity: sha512-test}
        "};
        assert!(
            first_new_peer_violation::<crate::lock::Pnpm>(None, split_pnpm_importers).is_none(),
            "disjoint pnpm importers keep their own contexts — no contract binds"
        );

        // The same declarations under npm: both packages hoist to the root `node_modules`, so
        // `plugin`'s nearest-ancestor lookup reaches the violating `host` copy no matter which
        // member declared it — declaration disjointness must not suppress the proven break.
        let hoisted_npm_members = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root" },
                "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
                "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
                "node_modules/plugin": {
                    "version": "1.0.0",
                    "peerDependencies": { "host": "^1.0.0" }
                },
                "node_modules/host": { "version": "2.0.0" }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, hoisted_npm_members).is_some(),
            "hoisted npm packages bind across disjoint members — the break is real"
        );

        // Physical isolation is what fails open on npm: host exists only inside apps/b's own
        // subtree, so plugin's ancestor lookup never reaches it and no contract binds.
        let isolated_npm_members = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root" },
                "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
                "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
                "node_modules/plugin": {
                    "version": "1.0.0",
                    "peerDependencies": { "host": "^1.0.0" }
                },
                "apps/b/node_modules/host": { "version": "2.0.0" }
            }
        }"#};
        assert!(
            first_new_peer_violation::<crate::lock::Npm>(None, isolated_npm_members).is_none(),
            "a peer copy the dependent cannot physically reach binds nothing"
        );

        let overlapping_pnpm = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/a:
                dependencies:
                  plugin:
                    specifier: ^1.0.0
                    version: 1.0.0
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

            packages:

              plugin@1.0.0:
                peerDependencies:
                  host: ^1.0.0

              host@2.0.0:
                resolution: {integrity: sha512-test}
        "};
        assert!(
            first_new_peer_violation::<crate::lock::Pnpm>(None, overlapping_pnpm).is_some(),
            "the same shape inside one importer is a genuine proven violation"
        );
    }

    /// Counterfactual attribution: the after-lock proves the *pair* incompatible, not who broke
    /// it. A dependent whose range did not change (`^1 || ^2`) is innocent when the peer jumped
    /// past it (1→3): the peer's candidate is rejected and the dependent's own move survives —
    /// dependent-first guessing would discard the maximal safe subset.
    #[test]
    fn peer_rejections_attribute_the_causally_culpable_candidate() {
        let before = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "plugin": "^1.0.0", "host": "^1.0.0" } },
                "node_modules/plugin": {
                    "version": "1.0.0",
                    "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
                },
                "node_modules/host": { "version": "1.0.0" }
            }
        }"#};
        let after = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "plugin": "^2.0.0", "host": "^3.0.0" } },
                "node_modules/plugin": {
                    "version": "2.0.0",
                    "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
                },
                "node_modules/host": { "version": "3.0.0" }
            }
        }"#};
        let baseline = PeerBaseline::gather::<crate::lock::Npm>(Some(before), &[]);
        let current = proven_peer_violations::<crate::lock::Npm>(after, &[]);
        let active = plan_of(vec![
            change("plugin", "1.0.0", "2.0.0"),
            change("host", "1.0.0", "3.0.0"),
        ]);

        let rejections = plan_peer_rejections(&baseline, &current, &active, &HashSet::new())
            .expect("uniquely attributable");
        assert_eq!(rejections.len(), 1, "exactly the culpable candidate");
        assert_eq!(
            rejections.first().map(|rejection| rejection.index),
            Some(1),
            "host (the peer whose jump the unchanged old range provably excludes) is rejected"
        );
        assert_eq!(
            rejections
                .first()
                .map(|rejection| rejection.offending.as_str()),
            Some("plugin"),
            "the rejection blames the contract's other party"
        );

        // The mirror shape: the dependent's NEW range excludes even the old peer — the dependent
        // is independently culpable and the peer's own move survives.
        let dependent_broke = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "plugin": "^2.0.0", "host": "^1.0.0" } },
                "node_modules/plugin": {
                    "version": "2.0.0",
                    "peerDependencies": { "host": "^9.0.0" }
                },
                "node_modules/host": { "version": "1.0.0" }
            }
        }"#};
        let current = proven_peer_violations::<crate::lock::Npm>(dependent_broke, &[]);
        let rejections = plan_peer_rejections(&baseline, &current, &active, &HashSet::new())
            .expect("uniquely attributable");
        assert_eq!(
            rejections.first().map(|rejection| rejection.index),
            Some(0),
            "plugin (whose new range excludes even the old host) is rejected"
        );

        // Neither side uniquely provable (the new range admits the old peer AND the old range
        // admits the new peer — an interaction only the pair exhibits): rejection would be a
        // guess, so the round aborts for candidate isolation.
        let interaction = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "fixture", "dependencies": { "plugin": "^2.0.0", "host": "^2.0.0" } },
                "node_modules/plugin": {
                    "version": "2.0.0",
                    "peerDependencies": { "host": "^1.0.0" }
                },
                "node_modules/host": { "version": "2.0.0" }
            }
        }"#};
        let current = proven_peer_violations::<crate::lock::Npm>(interaction, &[]);
        let active = plan_of(vec![
            change("plugin", "1.0.0", "2.0.0"),
            change("host", "1.0.0", "2.0.0"),
        ]);
        assert!(
            plan_peer_rejections(&baseline, &current, &active, &HashSet::new()).is_err(),
            "an interaction violation must go to candidate isolation, not a guess"
        );
    }

    /// npm's pre-apply gate judges visibility physically: hoisting lets a dependent declared by a
    /// *different* member bind the moving copy at the root `node_modules` (declaration
    /// disjointness holds nothing back), while a dependent whose own nested copy shadows the
    /// moving one is out of reach and must not hold it.
    #[test]
    fn peer_gate_judges_npm_visibility_physically() {
        let member_ref = |name: &str, path: &str| MemberRef {
            name: name.to_string(),
            path: path.to_string(),
        };
        let hoisted = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root" },
                "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
                "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
                "node_modules/plugin": {
                    "version": "1.0.0",
                    "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
                },
                "node_modules/host": { "version": "2.0.0" }
            }
        }"#};
        let mut host = change("host", "2.0.0", "3.0.0");
        host.members = vec![member_ref("b", "apps/b")];
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Npm>(&plan_of(vec![host.clone()]), Some(hoisted));
        assert!(
            retained.changes.is_empty(),
            "the hoisted contract binds across disjoint members"
        );
        assert_eq!(
            skipped.first().map(|skip| skip.reason),
            Some(SkipReason::PeerHeld)
        );

        // The dependent's own nested copy shadows the moving root instance: `plugin` never
        // resolves the copy this change rewrites, so nothing binds and the move is free.
        let shadowed = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root" },
                "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
                "apps/b": { "name": "b", "dependencies": { "host": "^2.0.0" } },
                "node_modules/plugin": {
                    "version": "1.0.0",
                    "peerDependencies": { "host": "^1.0.0 || ^2.0.0" }
                },
                "node_modules/plugin/node_modules/host": { "version": "1.0.0" },
                "node_modules/host": { "version": "2.0.0" }
            }
        }"#};
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Npm>(&plan_of(vec![host]), Some(shadowed));
        assert_eq!(
            retained.changes.len(),
            1,
            "a physically shadowed dependent holds nothing: {skipped:?}"
        );

        // Directory identity, not version equality: the dependent binds its own nested host@1
        // while the change rewrites apps/b's separate host@1 — same version, different physical
        // copy, so the plugin's copy survives the move untouched and must not hold it.
        let same_version_elsewhere = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root" },
                "apps/a": { "name": "a", "dependencies": { "plugin": "^1.0.0" } },
                "apps/b": { "name": "b", "dependencies": { "host": "^1.0.0" } },
                "node_modules/plugin": {
                    "version": "1.0.0",
                    "peerDependencies": { "host": "^1.0.0" }
                },
                "node_modules/plugin/node_modules/host": { "version": "1.0.0" },
                "apps/b/node_modules/host": { "version": "1.0.0" }
            }
        }"#};
        let mut scoped = change("host", "1.0.0", "2.0.0");
        scoped.members = vec![member_ref("b", "apps/b")];
        let PeerPartition { retained, skipped } = peer_partition::<crate::lock::Npm>(
            &plan_of(vec![scoped]),
            Some(same_version_elsewhere),
        );
        assert_eq!(
            retained.changes.len(),
            1,
            "an unrelated same-version copy must not conflate into a hold: {skipped:?}"
        );
    }

    /// The workspace-manifest source obeys the same layout rule as the lock source: under npm's
    /// hoisted tree, the local package's own directory resolves the moving copy no matter which
    /// member the change is scoped to, so context disjointness holds nothing back — while pnpm's
    /// isolated layout keeps the disjoint change out of the contract's reach.
    #[test]
    fn workspace_manifest_peer_judges_npm_visibility_physically() {
        let lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root-app" },
                "apps/site": { "name": "site", "dependencies": { "eslint": "^8.40.0" } },
                "packages/shim": { "name": "local-eslint-shim", "version": "0.1.0" },
                "node_modules/local-eslint-shim": { "resolved": "packages/shim", "link": true },
                "node_modules/eslint": { "version": "8.57.0" }
            }
        }"#};
        let install = crate::lock::Npm::install_paths(lock);
        let shim_peer = || WorkspacePeer {
            requirement: crate::lock::PeerRequirement {
                dependent: "local-eslint-shim".to_string(),
                dependent_version: "0.1.0".to_string(),
                package: "eslint".to_string(),
                range: "^8.0.0".to_string(),
            },
            origin: "packages/shim".to_string(),
            contexts: vec!["packages/shim".to_string()],
        };
        let mut eslint = change("eslint", "8.57.0", "9.0.0");
        eslint.members = vec![MemberRef {
            name: "site".to_string(),
            path: "apps/site".to_string(),
        }];

        assert!(
            workspace_peer_hold(&eslint, &[shim_peer()], &HashSet::new(), install.as_ref())
                .is_some(),
            "the shim's directory resolves the hoisted eslint the disjoint change rewrites"
        );
        assert!(
            workspace_peer_hold(&eslint, &[shim_peer()], &HashSet::new(), None).is_none(),
            "without a physical layout the disjoint contexts keep the contract out of reach"
        );

        // Directory identity again: the shim binds its OWN nested eslint copy while the change
        // rewrites apps/site's separate copy at the same version — the shim's copy survives the
        // move, so nothing may hold.
        let nested_lock = indoc! {r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root-app" },
                "apps/site": { "name": "site", "dependencies": { "eslint": "^8.40.0" } },
                "packages/shim": { "name": "local-eslint-shim", "version": "0.1.0" },
                "node_modules/local-eslint-shim": { "resolved": "packages/shim", "link": true },
                "packages/shim/node_modules/eslint": { "version": "8.57.0" },
                "apps/site/node_modules/eslint": { "version": "8.57.0" }
            }
        }"#};
        let nested = crate::lock::Npm::install_paths(nested_lock);
        assert!(
            workspace_peer_hold(&eslint, &[shim_peer()], &HashSet::new(), nested.as_ref())
                .is_none(),
            "an unrelated same-version copy must not conflate into a workspace-manifest hold"
        );
    }

    /// One contract shape, several instances: violation identity is per dependent instance, and
    /// baseline coverage is per binding member. A split copy floated onto the broken range in a
    /// member the baseline never covered is a NEW break; a dependent merely re-recorded at a new
    /// patch (same member, same range, contract already broken) introduces nothing.
    #[test]
    fn post_apply_diff_distinguishes_instances_of_one_contract_shape() {
        let before = split_shape_before();
        // apps/b's plugin floats 2 → 3; the new range is the SAME string apps/a's broken copy
        // already recorded (`^1.0.0`), so a shape-keyed diff would collapse the two instances
        // and grandfather the fresh break.
        let after = split_shape_after();
        let violation = first_new_peer_violation::<crate::lock::Pnpm>(Some(&before), &after)
            .expect("the apps/b float is a new break even under an old shape");
        assert_eq!(
            (
                violation.dependent.as_str(),
                violation.dependent_version.as_str()
            ),
            ("plugin", "3.0.0"),
            "the fresh instance, not apps/a's grandfathered one, is attributed"
        );

        // The counterpart: apps/a's already-broken copy re-recorded at a patch bump stays
        // grandfathered — same member, same range, nothing newly broken.
        let rerecorded = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/a:
                dependencies:
                  plugin:
                    specifier: ^1.0.0
                    version: 1.0.1
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

              apps/b:
                dependencies:
                  plugin:
                    specifier: ^2.0.0
                    version: 2.0.0
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

            packages:

              plugin@1.0.1:
                resolution: {integrity: sha512-p11}
                peerDependencies:
                  host: ^1.0.0

              plugin@2.0.0:
                resolution: {integrity: sha512-p2}
                peerDependencies:
                  host: ^1.0.0 || ^2.0.0

              host@2.0.0:
                resolution: {integrity: sha512-h2}
        "};
        assert!(
            first_new_peer_violation::<crate::lock::Pnpm>(Some(&before), rerecorded).is_none(),
            "a re-recorded instance of an already-broken contract is not re-attributed"
        );
    }

    /// Culprit matching is instance-aware. The rejected candidate must have LANDED the violating
    /// instance — same name, exact landed version, overlapping member — so a same-named change in
    /// another member is never blamed for it; and a multi-version name, which the whole-graph
    /// resolve deliberately never pins ([`prepare_whole_graph_inputs`]), is no candidate at all —
    /// implicating it aborts to candidate isolation instead of uselessly rejecting an unpinned
    /// change.
    #[test]
    fn peer_rejections_match_the_landed_instance_not_the_name() {
        let member_ref = |name: &str, path: &str| MemberRef {
            name: name.to_string(),
            path: path.to_string(),
        };
        let before = split_shape_before();
        let after = split_shape_after();
        let baseline = PeerBaseline::gather::<crate::lock::Pnpm>(Some(&before), &[]);
        let current = proven_peer_violations::<crate::lock::Pnpm>(&after, &[]);

        // Two same-named candidates: only the apps/b change landed the violating 3.0.0 instance.
        let mut decoy = change("plugin", "1.0.0", "1.5.0");
        decoy.members = vec![member_ref("a", "apps/a")];
        let mut mover = change("plugin", "2.0.0", "3.0.0");
        mover.members = vec![member_ref("b", "apps/b")];
        let active = plan_of(vec![decoy, mover]);
        let rejections = plan_peer_rejections(&baseline, &current, &active, &HashSet::new())
            .expect("uniquely attributable");
        assert_eq!(
            rejections.first().map(|rejection| rejection.index),
            Some(1),
            "the landed instance's own change is rejected, never the same-named decoy"
        );
        assert_eq!(rejections.len(), 1);
        assert_eq!(
            rejections
                .first()
                .map(|rejection| rejection.offending.as_str()),
            Some("host")
        );

        // The same violation with `plugin` multi-version — the resolve never pinned it, its
        // float is resolver latitude, and the peer did not move either: nobody is uniquely
        // culpable, so the round aborts to candidate isolation.
        let multi: HashSet<String> = std::iter::once("plugin".to_string()).collect();
        assert!(
            plan_peer_rejections(&baseline, &current, &active, &multi).is_err(),
            "an unpinned multi-version name must not be rejected as a candidate"
        );
    }

    /// The pnpm lock behind the split-instance tests: apps/a's `plugin@1` already violates
    /// against the shared `host@2` (grandfathered), while apps/b's `plugin@2` range still admits
    /// it.
    fn split_shape_before() -> String {
        indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/a:
                dependencies:
                  plugin:
                    specifier: ^1.0.0
                    version: 1.0.0
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

              apps/b:
                dependencies:
                  plugin:
                    specifier: ^2.0.0
                    version: 2.0.0
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

            packages:

              plugin@1.0.0:
                resolution: {integrity: sha512-p1}
                peerDependencies:
                  host: ^1.0.0

              plugin@2.0.0:
                resolution: {integrity: sha512-p2}
                peerDependencies:
                  host: ^1.0.0 || ^2.0.0

              host@2.0.0:
                resolution: {integrity: sha512-h2}
        "}
        .to_string()
    }

    /// [`split_shape_before`] after apps/b's plugin floated to 3.0.0, whose range re-records the
    /// same `^1.0.0` string apps/a's copy already carries.
    fn split_shape_after() -> String {
        indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/a:
                dependencies:
                  plugin:
                    specifier: ^1.0.0
                    version: 1.0.0
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

              apps/b:
                dependencies:
                  plugin:
                    specifier: ^3.0.0
                    version: 3.0.0
                  host:
                    specifier: ^2.0.0
                    version: 2.0.0

            packages:

              plugin@1.0.0:
                resolution: {integrity: sha512-p1}
                peerDependencies:
                  host: ^1.0.0

              plugin@3.0.0:
                resolution: {integrity: sha512-p3}
                peerDependencies:
                  host: ^1.0.0

              host@2.0.0:
                resolution: {integrity: sha512-h2}
        "}
        .to_string()
    }

    /// The moving-dependent exemption is recomputed to a fixed point: a dependent whose own move is
    /// peer-held stays in place, so it stops exempting the package it pins — the plan cannot leak a
    /// break through a co-move that never happens.
    #[test]
    fn peer_gate_recomputes_when_the_exempting_dependent_is_itself_held() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              .:
                dependencies:
                  fumadocs-core:
                    specifier: ^16.0.0
                    version: 16.11.4
                  fumadocs-mdx:
                    specifier: ^15.0.0
                    version: 15.1.1(fumadocs-core@16.11.4)
                  docs-kit:
                    specifier: ^1.0.0
                    version: 1.0.0(fumadocs-mdx@15.1.1)

            packages:

              fumadocs-core@16.11.4:
                resolution: {integrity: sha512-aaa}

              fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
                resolution: {integrity: sha512-bbb}
                peerDependencies:
                  fumadocs-core: ^16.0.0

              docs-kit@1.0.0(fumadocs-mdx@15.1.1):
                resolution: {integrity: sha512-ccc}
                peerDependencies:
                  fumadocs-mdx: ^15.0.0
        "};

        // mdx's own move is held by docs-kit's peer range, so mdx stays at 15.1.1 — and the second
        // round therefore holds core, which round one had exempted for mdx's planned co-move.
        let plan = plan_of(vec![
            change("fumadocs-core", "16.11.4", "17.0.0"),
            change("fumadocs-mdx", "15.1.1", "16.0.0"),
        ]);
        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan, Some(lock));
        assert!(retained.changes.is_empty());
        let blame_for = |name: &str| {
            skipped
                .iter()
                .find(|held| held.change.package.name == name)
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str())
        };
        assert_eq!(blame_for("fumadocs-mdx"), Some("docs-kit"));
        assert_eq!(blame_for("fumadocs-core"), Some("fumadocs-mdx"));
    }

    /// A same-name move in a *disjoint* importer never exempts: the held copy's importer keeps its
    /// version, so the peer range still binds there. (The moving copy here is also a
    /// multi-version-declared name, which the resolve later skips as `MultiVersionHeld` — one more
    /// reason it must not count as moving.)
    #[test]
    fn peer_gate_ignores_a_same_name_move_in_a_disjoint_importer() {
        let lock = indoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/site:
                dependencies:
                  fumadocs-mdx:
                    specifier: ^16.0.0
                    version: 16.0.0

              apps/docs:
                dependencies:
                  fumadocs-core:
                    specifier: ^16.0.0
                    version: 16.11.4
                  fumadocs-mdx:
                    specifier: ~15.1.0
                    version: 15.1.1(fumadocs-core@16.11.4)

            packages:

              fumadocs-core@16.11.4:
                resolution: {integrity: sha512-aaa}

              fumadocs-mdx@15.1.1(fumadocs-core@16.11.4):
                resolution: {integrity: sha512-bbb}
                peerDependencies:
                  fumadocs-core: ^16.0.0

              fumadocs-mdx@16.0.0:
                resolution: {integrity: sha512-ccc}
        "};
        let member = |path: &str| MemberRef {
            name: path.to_string(),
            path: path.to_string(),
        };

        let mut site_mdx = change("fumadocs-mdx", "16.0.0", "16.2.0");
        site_mdx.members = vec![member("apps/site")];
        let mut docs_core = change("fumadocs-core", "16.11.4", "17.0.0");
        docs_core.members = vec![member("apps/docs")];

        let PeerPartition { retained, skipped } =
            peer_partition::<crate::lock::Pnpm>(&plan_of(vec![docs_core, site_mdx]), Some(lock));
        // The mdx move in apps/site cannot lift apps/docs's mdx@15.1.1 peer pin on core.
        assert_eq!(retained.changes.len(), 1, "only the mdx move survives");
        assert_eq!(
            skipped
                .first()
                .and_then(|held| held.offending.as_ref())
                .map(|package| package.name.as_str()),
            Some("fumadocs-mdx")
        );
        assert_eq!(
            skipped
                .first()
                .map(|held| held.change.package.name.as_str()),
            Some("fumadocs-core")
        );
    }

    /// A [`Project`] rooted in a temporary directory whose `package.json` declares `nanoid`.
    struct DeclaredProject {
        /// Owns the temporary root directory; dropping it deletes the project on disk, so it must
        /// stay bound for as long as the project is used.
        guard: tempfile::TempDir,
        /// The project whose root and manifest live inside the temporary directory.
        project: Project,
    }

    fn project_declaring(spec: &str) -> DeclaredProject {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            format!(r#"{{ "dependencies": {{ "nanoid": "{spec}" }} }}"#),
        )
        .expect("write manifest");
        let project = Project {
            root: root.clone(),
            kind: crate::lock::Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        DeclaredProject {
            guard: dir,
            project,
        }
    }

    #[test]
    fn pnpm_uses_lock_only_only_for_in_range_auto() {
        let DeclaredProject {
            guard: _guard,
            project,
        } = project_declaring("^3.0.0");

        // In-range minor under Auto → lock-only `pnpm update --no-save` (the declared range stands).
        let in_range = change("nanoid", "3.1.0", "3.3.0");
        let args = lockonly_command::<crate::lock::Pnpm>(&project, &in_range, RewriteMode::Auto)
            .expect("command");
        assert_eq!(
            args,
            Some(vec![
                "update".to_string(),
                "nanoid@3.3.0".to_string(),
                "--lockfile-only".to_string(),
                "--no-save".to_string()
            ])
        );

        // Out-of-range and `--rewrite` both take the manifest-rewrite + relock path.
        let major = change("nanoid", "3.1.0", "5.0.0");
        assert!(
            lockonly_command::<crate::lock::Pnpm>(&project, &major, RewriteMode::Auto)
                .expect("cmd")
                .is_none()
        );
        assert!(
            lockonly_command::<crate::lock::Pnpm>(&project, &in_range, RewriteMode::Always)
                .expect("command")
                .is_none()
        );
        assert_eq!(
            crate::lock::Pnpm::relock_args(None),
            ["install", "--lockfile-only"]
        );
    }

    #[test]
    fn relock_commands_refresh_locks_without_adding_dependencies() {
        assert_eq!(
            crate::lock::Npm::relock_args(None),
            ["install", "--package-lock-only", "--no-audit", "--no-fund"]
        );
        assert_eq!(
            crate::lock::Pnpm::relock_args(None),
            ["install", "--lockfile-only"]
        );
        assert_eq!(crate::lock::Yarn::relock_args(None), ["install"]);
        assert_eq!(crate::lock::Bun::relock_args(None), ["install"]);
    }

    #[test]
    fn npm_install_commands_apply_the_absolute_before_cutoff() {
        let before = Some("2024-08-01T00:00:00Z");

        assert_eq!(
            Npm::relock_args(before),
            [
                "install",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert_eq!(
            Npm::pinned_relock_args("eslint", "10.6.0", before).expect("npm supports exact pins"),
            [
                "install",
                "eslint@10.6.0",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert_eq!(
            Npm::build_args(before),
            [
                "install",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
    }

    /// npm's save-capable exact pin is bracketed around the manifest bytes cooldown authorized,
    /// including when cooldown widened the range before taking the snapshot.
    #[test]
    fn npm_restores_the_authorized_widened_range_after_its_exact_pin() {
        let DeclaredProject {
            guard: _guard,
            mut project,
        } = project_declaring("^3.0.0");
        project.exclude_newer = Some("2024-08-01T00:00:00Z".to_string());
        let change = change("nanoid", "3.1.0", "5.1.11");

        let landing = candidate_landing::<Npm>(&project, &change, RewriteMode::Auto)
            .expect("landing")
            .expect("declared candidate");
        let CandidateLanding::PinRestoreResync {
            pin,
            authorized_manifests,
            resync,
        } = landing
        else {
            panic!("npm's save-capable pin must be bracketed")
        };
        assert_eq!(
            pin,
            [
                "install",
                "nanoid@5.1.11",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert_eq!(
            resync,
            [
                "install",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--before=2024-08-01T00:00:00Z"
            ]
        );
        assert!(
            std::fs::read_to_string(&project.manifest)
                .expect("read widened manifest")
                .contains(r#""nanoid": "^5.1.11""#),
            "cooldown applies the authorized range before npm runs"
        );

        std::fs::write(
            &project.manifest,
            r#"{ "dependencies": { "nanoid": "^5.1.11-overwritten" } }"#,
        )
        .expect("simulate npm save");
        authorized_manifests
            .restore(&project.root)
            .expect("restore authorized bytes");
        assert_eq!(
            std::fs::read_to_string(&project.manifest).expect("read restored manifest"),
            r#"{ "dependencies": { "nanoid": "^5.1.11" } }"#,
            "the snapshot restores cooldown's range, not the pre-widen or npm-authored bytes"
        );
    }

    #[test]
    fn npm_authorizes_a_target_steering_member_edit_when_every_range_is_compatible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("apps/app")).expect("member directory");
        std::fs::write(
            root.join("package.json"),
            r#"{ "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" } }"#,
        )
        .expect("root manifest");
        std::fs::write(
            root.join("apps/app/package.json"),
            r#"{ "devDependencies": { "chalk": "^5.6.0" } }"#,
        )
        .expect("member manifest");
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut change = change("chalk", "5.6.0", "5.6.2");
        change.members = vec![
            MemberRef {
                name: "root".into(),
                path: ".".into(),
            },
            MemberRef {
                name: "app".into(),
                path: "apps/app".into(),
            },
        ];

        let landing = candidate_landing::<Npm>(&project, &change, RewriteMode::Auto)
            .expect("landing")
            .expect("declared candidate");

        let CandidateLanding::PinRestoreResync { pin, resync, .. } = landing else {
            panic!("npm's member-owned exact pin must be bracketed")
        };
        assert_eq!(
            pin,
            [
                "install",
                "chalk@5.6.2",
                "--package-lock-only",
                "--no-audit",
                "--no-fund",
                "--workspace=apps/app"
            ]
        );
        assert_eq!(
            resync,
            ["install", "--package-lock-only", "--no-audit", "--no-fund"]
        );
        assert!(
            std::fs::read_to_string(root.join("apps/app/package.json"))
                .expect("read member")
                .contains(r#""chalk": "^5.6.2""#),
            "the member edit keeps npm's restored lock on the exact target"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("package.json")).expect("read root"),
            r#"{ "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" } }"#,
            "the root's published contract remains byte-identical"
        );
    }

    /// A declaration nothing may rewrite (a published `peerDependencies` contract) leaves an empty
    /// widen write set, and a plain relock cannot move a lock that still satisfies its range — so
    /// the landing is an EXACT manifest-preserving pin. It must be target-directed, not a
    /// "newest the range admits" update: under a broad `>=5 <7` range, a deliberate patch move
    /// (`--major` off) would overshoot the plan and be rejected, and a `fix` rollback could not move
    /// downward at all.
    #[test]
    fn npm_pins_a_peer_only_declaration_exactly_and_restores_the_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            r#"{ "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" } }"#,
        )
        .expect("write manifest");
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let change = change("chalk", "5.6.0", "5.6.2");
        let declarations =
            manifest::declarations(&project.root, &change.members, &change.package.name)
                .expect("declarations");
        assert_eq!(
            (
                declarations.absent(),
                declarations.has_install(),
                declarations.peer
            ),
            (false, false, true),
            "a peer-only declaration is declared, but writable nowhere"
        );

        // The landing is the bracketed exact pin: the pin names the exact planned version, and the
        // plain resync recopies metadata from the restored manifest.
        let pin = preserving_pin::<Npm>(&project, &change, &[]).expect("npm can pin exactly");
        match pin {
            crate::lock::PreservingPin::PinRestoreResync { pin, resync } => {
                assert_eq!(
                    pin,
                    [
                        "install",
                        "chalk@5.6.2",
                        "--package-lock-only",
                        "--no-audit",
                        "--no-fund"
                    ],
                    "the pin is target-directed, not a range-maximum update"
                );
                assert_eq!(
                    resync,
                    ["install", "--package-lock-only", "--no-audit", "--no-fund"],
                    "the resync recopies restored manifest metadata without retargeting the lock"
                );
            }
            crate::lock::PreservingPin::Direct(args) => {
                panic!("npm's exact pin saves the range, so it must be bracketed: {args:?}")
            }
        }

        // pnpm needs no bracketing: its exact pin already skips the manifest.
        std::assert_matches!(
            preserving_pin::<crate::lock::Pnpm>(&project, &change, &[]),
            Some(crate::lock::PreservingPin::Direct(_))
        );
    }

    #[test]
    fn pnpm_lock_only_requires_all_declaring_manifests_to_accept_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("apps/a")).expect("mkdir a");
        std::fs::create_dir_all(root.join("apps/b")).expect("mkdir b");
        std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#).expect("root manifest");
        std::fs::write(
            root.join("apps/a/package.json"),
            r#"{ "dependencies": { "nanoid": "^3.0.0" } }"#,
        )
        .expect("manifest a");
        std::fs::write(
            root.join("apps/b/package.json"),
            r#"{ "dependencies": { "nanoid": "^2.0.0" } }"#,
        )
        .expect("manifest b");
        let project = Project {
            root: root.clone(),
            kind: crate::lock::Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut change = change("nanoid", "3.1.0", "3.3.0");
        change.members = vec![
            MemberRef {
                name: "a".into(),
                path: "apps/a".into(),
            },
            MemberRef {
                name: "b".into(),
                path: "apps/b".into(),
            },
        ];

        let args = lockonly_command::<crate::lock::Pnpm>(&project, &change, RewriteMode::Auto)
            .expect("cmd");

        assert!(args.is_none());
    }

    #[test]
    fn npm_has_no_direct_lock_only_command() {
        let DeclaredProject {
            guard: _guard,
            project,
        } = project_declaring("^3.0.0");
        let in_range = change("nanoid", "3.1.0", "3.3.0");
        assert!(
            lockonly_command::<Npm>(&project, &in_range, RewriteMode::Auto)
                .expect("command")
                .is_none()
        );
    }

    #[tokio::test]
    async fn apply_skips_when_no_declaring_manifest_entry_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#).expect("manifest");
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

        let report = tool()
            .apply(&project, &plan, &ProjectMutationJournal::default())
            .await
            .expect("apply");

        assert!(report.applied.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::NotEligible);
        let manifest = std::fs::read_to_string(root.join("package.json")).expect("read manifest");
        assert_eq!(manifest, r#"{ "name": "root" }"#);
    }
}
