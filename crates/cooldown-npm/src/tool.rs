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
use crate::lock::{EffectiveRegistryQuery, MemberIndex, NameVersion, NodeLock};
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
    ApplyReport, CandidateScope, Capabilities, Change, CoreError, DepScope, Dependency, Diagnostic,
    DiagnosticKind, FetchContext, LockStatus, LockVerifyReport, MemberRef, NativePolicyLayer,
    PackageId, PackageRegistry, Plan, PreparedMutation, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality,
    ResolvedPolicy, Result, RewriteMode, SkipReason, Skipped, SyncReport, SyncScope, ToolId,
    ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
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
///
/// The strict parse failure propagates instead of defaulting to an empty map: an empty default
/// would diff a present-but-corrupted lock as "nothing resolved", silently reporting every
/// candidate unmoved/unreached — the healthy-looking failure the fail-closed [`NodeLock::parse`]
/// contract exists to prevent.
/// A caller with legitimately absent content decides fail-open at its own call site.
fn locked_versions<L: NodeLock>(content: &str) -> Result<HashMap<String, String>> {
    let mut versions: HashMap<String, String> = HashMap::new();
    for NameVersion { name, version, .. } in L::parse(content)? {
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
    Ok(versions)
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
    /// The effective registry routing per project root, queried from the manager binary once
    /// per run when advisory identities need confirming (`None` cached for a failed query, so
    /// a broken binary is asked once, not once per gate pass).
    effective_registry: tokio::sync::Mutex<
        std::collections::HashMap<Utf8PathBuf, Option<crate::npmrc::RegistryOverrides>>,
    >,
    _lock: PhantomData<fn() -> L>,
}

impl<L: NodeLock> NpmTool<L> {
    /// Creates the adapter from a configured [`NpmRegistry`].
    #[must_use]
    pub fn new(registry: NpmRegistry) -> Self {
        NpmTool {
            registry,
            cmd: NodeCmd::new(L::BIN),
            effective_registry: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            _lock: PhantomData,
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`NpmRegistry`].
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        NpmTool::new(NpmRegistry::new(http))
    }

    /// The manager's effective registry overrides for `root`, asked of the binary once and memoized
    /// — including a failed query, which yields `None` (withhold everything) rather than a retry
    /// per gate pass.
    /// A manager with no such query ([`EffectiveRegistryQuery::Unavailable`]) is never spawned and
    /// confirms nothing.
    async fn effective_overrides(
        &self,
        root: &Utf8Path,
    ) -> Option<crate::npmrc::RegistryOverrides> {
        if L::EFFECTIVE_REGISTRY == EffectiveRegistryQuery::Unavailable {
            return None;
        }
        let mut cache = self.effective_registry.lock().await;
        if let Some(cached) = cache.get(root) {
            return cached.clone();
        }
        let args: Vec<String> = ["config", "list", "--json"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let queried = match self.cmd.stdout(root, &args).await {
            Ok(stdout) => {
                crate::npmrc::overrides_from_effective_config(&stdout, L::EFFECTIVE_REGISTRY)
            }
            Err(_) => None,
        };
        cache.insert(root.to_owned(), queried.clone());
        queried
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

    async fn rescope_members(
        &self,
        project: &Project,
        deps: &mut [Dependency],
        excluded: &[MemberRef],
    ) -> Result<()> {
        // The same per-importer records `dependencies` derived these facts from, minus the excluded
        // importers: an excluded importer's exact pin or range must neither hold nor loosen a row
        // the run manages.
        let content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let excluded: HashSet<String> = excluded.iter().map(|member| member.path.clone()).collect();
        let index = L::member_sources_excluding(&content, &excluded);
        for dep in deps
            .iter_mut()
            .filter(|dep| dep.direct && !dep.members.is_empty())
        {
            let name = dep.package.name.as_str();
            dep.pinned = index.is_exact_pinned(name, dep.current.as_str());
            dep.declared_bound = manifest::declared_bound(&project.root, &dep.members, name)?;
        }
        Ok(())
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
            advisory_ecosystem: Some("npm"),
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

    /// The lock's origin evidence says where the *locked* artifact came from; adopting a shortened
    /// window fetches a *future* target under the manager's effective registry routing, whose
    /// global and builtin layers no file walk can locate.
    /// So at feed time the binary itself is asked (`<bin> config list --json`, memoized per root)
    /// and every identity it reroutes — or, when the query fails or the manager offers no such
    /// query (yarn) — every identity at all, is withheld.
    /// For pnpm, whose lock names no registry per entry, the query is the other half of the proof
    /// rather than a veto: the effective `registry` must be stated and public
    /// ([`EffectiveRegistryQuery::Proves`]).
    async fn confirm_advisory_identities(&self, project: &Project, deps: &mut [Dependency]) {
        if deps.iter().all(|dep| dep.advisory_identity.is_none()) {
            return;
        }
        match self.effective_overrides(&project.root).await {
            Some(overrides) => {
                for dep in deps {
                    if dep.advisory_identity.is_some() && overrides.reroutes(&dep.package.name) {
                        dep.advisory_identity = None;
                    }
                }
            }
            None => {
                for dep in deps {
                    dep.advisory_identity = None;
                }
            }
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let entries = L::parse(&content)?;
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

        // Advisory identity needs positive origin evidence: the lock entry's own record — npm's and
        // yarn's `resolved` URL naming the public npm registry, or pnpm's registry-only resolution
        // shape, whose registry the feed-time query then has to name (see
        // `confirm_advisory_identities`).
        // A format without a per-entry record (bun) grants nothing; the config layers can only veto
        // a granted identity, never substitute for the record.
        let registry_overrides =
            crate::npmrc::RegistryOverrides::read(&project.root, L::NATIVE_MIN_AGE_FILE);
        let mut seen = HashSet::new();
        let mut deps = Vec::new();
        for NameVersion {
            name,
            version,
            origin,
        } in entries
        {
            // A non-registry resolution — an injected workspace package (`file:`/`link:`), a git
            // or tarball URL — has no registry release history to evaluate and, like cargo's
            // non-registry sources, is not cooldown's to move. The `:` discriminates: registry
            // versions are semver strings, which never contain one.
            if version.contains(':') {
                continue;
            }
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
            let advisory_identity =
                crate::npmrc::advisory_identity(&name, &origin, &registry_overrides);
            deps.push(Dependency {
                package: PackageId::new(L::ID, name, Some(NPM.to_string())),
                advisory_identity,
                current: Version::new(version.clone()),
                current_quality: classify_quality(&version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
                declared_bound,
                members,
                pinned,
                hold_edges: Vec::new(),
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
            .lock_report(&project.root, &args, L::LOCKFILE)
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

/// Whether the lock's version for `name` actually moved across a whole-graph resolve.
///
/// A name can resolve to several copies in a pnpm graph; the `before`/`after` maps track its
/// *newest* copy, so a candidate planned off a stale duplicate whose newest copy already sits at
/// the target shows no net move. Reporting only genuine moves keeps the report set equal to the
/// lock-diff set: a converged re-run, where nothing moved, reports zero applied (no oscillation).
/// The newest copy alone is blind in the other direction, though: an importer copy that genuinely
/// moved beneath a newer transitive duplicate shows no net newest-copy change, and its applied row
/// would vanish. The importer-resolved version sets restore that visibility (empty on both sides
/// for npm's name-only index, which keeps its newest-copy judgment); a converged re-run still
/// moves neither.
fn candidate_moved(
    name: &str,
    before: &HashMap<String, String>,
    after: &HashMap<String, String>,
    before_members: &crate::lock::MemberIndex,
    after_members: &crate::lock::MemberIndex,
) -> bool {
    before_members.resolved_versions_of(name) != after_members.resolved_versions_of(name)
        || match (before.get(name), after.get(name)) {
            (Some(from), Some(to)) => version::compare(from, to).is_ne(),
            (None, Some(_)) | (Some(_), None) => true,
            (None, None) => false,
        }
}

/// Whether the re-locked graph resolves `change` at exactly its target, judged per declaring
/// member when the lock carries member-scoped entries, per the member's physical install-tree
/// instance when the lock records layout (npm), and by the name's newest copy only as the last
/// resort.
///
/// A successful install command is not proof: `npm install <name>@<version> --before=<cutoff>`
/// exits 0 yet lands the newest pre-cutoff version when the requested one is newer than the
/// cutoff, so the landing must be read back from the lock.
fn exact_target_reached<L: NodeLock>(project: &Project, change: &Change) -> Result<bool> {
    let content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
    let newest = locked_versions::<L>(&content)?;
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
    // npm's declaration attribution is name-only, but its lock records the physical install tree:
    // judge each declaring member's own resolved instance — the nearest enclosing copy — so a
    // newer duplicate nested under another dependent cannot mask a landed root copy. The
    // newest-copy fallback below would read the duplicate, roll the genuine landing back as a
    // conflict, and do so again on every future run.
    if let Some(paths) = L::install_paths(&content) {
        let instances: Vec<Option<&str>> = change
            .members
            .iter()
            .map(|member| {
                paths
                    .member_resolution(&member.path, &change.package.name)
                    .map(|instance| instance.version)
            })
            .collect();
        if instances.iter().any(Option::is_some) {
            return Ok(instances.into_iter().all(|version| version == Some(target)));
        }
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

/// Whether `landed` is at or beyond `change`'s target, respecting the move's direction (a forward
/// move must reach at/above its target, a downgrade at/below it).
fn satisfies_target(change: &Change, landed: &str) -> bool {
    let ordering = version::compare(landed, change.to.as_str());
    if change.downgrade {
        ordering.is_le()
    } else {
        ordering.is_ge()
    }
}

/// Whether a planned candidate landed at or beyond its target in **every** declaring member.
///
/// Checked per declaring member, not against the name's newest copy: a multi-version dependency can
/// leave one member short of the target even though the name's newest copy — a higher line owned by
/// another member — already sits at it.
/// Checking only the newest copy would falsely report such a candidate as landed.
///
/// Every member must have reached the target: an exact-pinned candidate is all-or-nothing by
/// construction (the resolve loop rolls a partial landing back — [`partial_landings`]), and a
/// range-floated candidate the resolver moved in only some members has not landed the planned move
/// either — it is held, and the float itself surfaces as a collateral row.
/// Falls back to the newest copy when the change carries no member attribution (a collateral move)
/// or the lock has no per-member version data.
fn reached(
    after_newest: &HashMap<String, String>,
    after_members: &MemberIndex,
    change: &Change,
) -> bool {
    let name = change.package.name.as_str();
    if change.members.is_empty() {
        return after_newest
            .get(name)
            .is_some_and(|landed| satisfies_target(change, landed));
    }
    change.members.iter().all(|member| {
        after_members
            .resolved_version(&member.path, name)
            .is_some_and(|landed| satisfies_target(change, landed))
    })
}

/// An exact-pinned candidate the joint resolve landed in only some of its declaring importers — not
/// the move that was requested.
///
/// A pin is a request for *every* declaring importer: pnpm can leave one behind (a peer-bound
/// subtree that keeps its copy).
/// Committing that would split a name that resolved at one version before the run and report
/// nothing — so the candidate is rejected and the graph re-resolved without it
/// ([`NpmTool::resolve_and_verify`]).
/// Importers the run excludes are not part of this contract: pnpm re-resolves the named package in
/// every importer whose range admits it whatever the update's filter, so an excluded importer moving
/// is the resolver's doing and is reported by the settled-lock guard ([`report_excluded_moves`]).
struct PartialLanding {
    index: usize,
    /// The in-scope importers that reached the target.
    landed: Vec<String>,
    /// The in-scope importers that did not, with the version each sits at (`None`: no longer
    /// declared).
    stayed: Vec<(String, Option<String>)>,
}

/// Judges every exact-pinned candidate of `active` against the post-resolve importer index (see
/// [`PartialLanding`]).
/// A candidate the resolve was never asked to pin — a transitive advance, a held workspace split —
/// has no all-or-nothing contract to enforce, and one without member attribution has nothing to
/// judge per importer.
fn partial_landings(
    active: &Plan,
    multi_version: &HashSet<String>,
    after: &MemberIndex,
) -> Vec<PartialLanding> {
    let mut partial = Vec::new();
    for (index, change) in active.changes.iter().enumerate() {
        let name = change.package.name.as_str();
        if !change.direct || multi_version.contains(name) || change.members.is_empty() {
            continue;
        }
        let mut landed = Vec::new();
        let mut stayed = Vec::new();
        for member in &change.members {
            match after.resolved_version(&member.path, name) {
                Some(version) if satisfies_target(change, version) => {
                    landed.push(member.name.clone());
                }
                version => stayed.push((member.name.clone(), version.map(str::to_string))),
            }
        }
        if !landed.is_empty() && !stayed.is_empty() {
            partial.push(PartialLanding {
                index,
                landed,
                stayed,
            });
        }
    }
    partial
}

/// The skip row for a rolled-back partial landing: a resolver conflict that names the importers
/// that did and did not take the target, so the rollback is never mistaken for a plain rejection.
/// Blame goes to the peer-suffixed sibling that held the importer back when one is uniquely
/// identifiable, else to the candidate itself.
fn partial_landing_skip<L: NodeLock>(
    change: &Change,
    partial: &PartialLanding,
    after_content: &str,
) -> Skipped {
    let name = change.package.name.as_str();
    let target = change.to.as_str();
    let stayed = partial
        .stayed
        .iter()
        .map(|(member, version)| {
            format!(
                "{member} at {}",
                version.as_deref().unwrap_or("no resolved copy")
            )
        })
        .collect::<Vec<_>>();
    let offender = peer_conflict_blocker(after_content, name).unwrap_or_else(|| name.to_string());
    Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(PackageId::new(L::ID, offender, Some(NPM.to_string()))),
        detail: Some(format!(
            "the resolve landed {target} in {} of {} importers ({}) and left {}; rolled back rather than split {name} across importers",
            partial.landed.len(),
            partial.landed.len() + partial.stayed.len(),
            list_members(&partial.landed),
            list_members(&stayed),
        )),
    }
}

/// `a, b, c` for a short list, `a, b, c (+N others)` past three, so a detail line stays readable on
/// a wide workspace while still naming the importers it is about.
fn list_members(members: &[String]) -> String {
    const SHOWN: usize = 3;
    let shown = members
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match members.len().saturating_sub(SHOWN) {
        0 => shown,
        rest => format!("{shown} (+{rest} others)"),
    }
}

/// A name whose importers resolved it to exactly one version before the resolve and to several
/// after it — a duplicate copy the run introduced.
struct ImporterSplit {
    name: String,
    before: String,
    /// Every post-resolve version with the importers on it, ascending.
    after: Vec<(String, Vec<String>)>,
}

impl ImporterSplit {
    /// Whether the split is exactly what the run's exclusions asked for: the importers the run
    /// manages all resolve the name to one version, and every other version is the pre-apply one,
    /// held only by excluded importers that were left in place (out of range for the target, or
    /// peer-bound).
    /// An excluded importer pnpm re-resolved onto the in-scope version shares that bucket and is no
    /// split; its move is reported separately ([`report_excluded_moves`]).
    /// Anything else — two in-scope versions, an excluded importer on a third version — is a split
    /// nobody asked for.
    fn is_deliberate(&self, excluded: &HashSet<String>) -> bool {
        let in_scope: BTreeSet<&str> = self
            .after
            .iter()
            .filter(|(_, members)| members.iter().any(|member| !excluded.contains(member)))
            .map(|(version, _)| version.as_str())
            .collect();
        in_scope.len() <= 1
            && self.after.iter().all(|(version, members)| {
                in_scope.contains(version.as_str())
                    || (*version == self.before
                        && members.iter().all(|member| excluded.contains(member)))
            })
    }

    fn describe(&self) -> String {
        let after = self
            .after
            .iter()
            .map(|(version, members)| format!("{version} in {}", list_members(members)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} resolved at {} across the workspace before the run and at several versions after it: {after}",
            self.name, self.before
        )
    }
}

/// The importer-level splits the resolve introduced (see [`ImporterSplit`]): judged over every
/// importer that declares the name, excluded ones included, since the lock is what the workspace
/// installs.
/// Names already at several versions before the run keep their existing treatment.
fn importer_splits(before: &MemberIndex, after: &MemberIndex) -> Vec<ImporterSplit> {
    let mut names: Vec<String> = after.declared_names().into_iter().collect();
    names.sort();
    let mut splits = Vec::new();
    for name in names {
        let was_versions = before.resolved_versions_of(&name);
        let [was] = was_versions.as_slice() else {
            continue;
        };
        let now = after.resolved_versions_of(&name);
        if now.len() <= 1 {
            continue;
        }
        splits.push(ImporterSplit {
            before: (*was).to_string(),
            after: now
                .iter()
                .map(|version| ((*version).to_string(), after.members_for(&name, version)))
                .collect(),
            name,
        });
    }
    splits
}

/// Accounts for every importer-level split the settled resolve introduced ([`importer_splits`]).
///
/// The resolve loop has already rolled back every pin that landed in only some of its importers, so
/// a name the workspace resolved at one version and now resolves at several is the resolver's own
/// doing.
/// A split whose only importers left behind are ones the run excludes is what the exclusion asked
/// for and is reported as a warning naming them; any other split would commit a duplicate copy no
/// row accounts for, so it fails the batch — the caller's candidate isolation then names the
/// candidate whose landing caused it.
fn report_importer_splits(
    report: &mut ApplyReport,
    before: &MemberIndex,
    after: &MemberIndex,
    excluded: &HashSet<String>,
) -> Result<HashSet<String>> {
    let mut accounted = HashSet::new();
    for split in importer_splits(before, after) {
        if !split.is_deliberate(excluded) {
            return Err(CoreError::UnacceptableResolve(format!(
                "the resolve split a workspace dependency: {}",
                split.describe()
            )));
        }
        report.warnings.push(
            Diagnostic::new(
                DiagnosticKind::Held,
                format!(
                    "{}; the importer(s) still at {} are excluded from this run and were left in place",
                    split.describe(),
                    split.before
                ),
            )
            .with_package(split.name.clone()),
        );
        accounted.insert(split.name);
    }
    Ok(accounted)
}

/// Reports every name the settled resolve took from one resolved version to several across the
/// whole package graph that the importer-level guard did not already account for: a copy no
/// importer declares, pulled in by another package's own requirement.
/// Such a copy is the resolver's legitimate answer to that dependent's range, so it is committed,
/// but a lock that gained a second copy must never pass silently — the run's own report names the
/// versions, whatever the newest-copy diff shows.
fn report_graph_duplicates<L: NodeLock>(
    report: &mut ApplyReport,
    before: &str,
    after: &str,
    accounted: &HashSet<String>,
) -> Result<()> {
    let before = resolved_version_lines::<L>(before)?;
    let after = resolved_version_lines::<L>(after)?;
    let mut names: Vec<&String> = after.keys().collect();
    names.sort();
    for name in names {
        if accounted.contains(name) {
            continue;
        }
        let (Some(was), Some(now)) = (before.get(name), after.get(name)) else {
            continue;
        };
        let mut lines = was.iter();
        let (Some(single), None) = (lines.next(), lines.next()) else {
            continue;
        };
        if now.len() <= 1 {
            continue;
        }
        report.warnings.push(
            Diagnostic::new(
                DiagnosticKind::Held,
                format!(
                    "{name} resolved at one version ({single}) across the graph before the run and at several after it ({}): a copy no importer declares, pulled in by another package's requirement",
                    now.iter().cloned().collect::<Vec<_>>().join(", ")
                ),
            )
            .with_package(name.clone()),
        );
    }
    Ok(())
}

/// A direct entry the settled resolve changed in an importer the run excludes.
struct ExcludedMove {
    importer: String,
    name: String,
    /// `1.0.0 → 2.0.0`, `declared ^1 → ^2, 1.0.0 → 2.0.0`, `added at 2.0.0`, or `removed at 1.0.0`.
    movement: String,
}

impl ExcludedMove {
    fn describe(&self) -> String {
        format!("{} in {} ({})", self.name, self.importer, self.movement)
    }
}

/// The direct entries the settled resolve changed in importers the run excludes, ascending by
/// importer and name, sorted into what the resolver does on its own and what it does not.
///
/// `pnpm update <name>@<target>` re-resolves the named package in *every* importer whose declared
/// range admits a newer version, whatever `--filter` it carried: the filtered importers get the
/// exact target, every other one the newest version its own range admits under the release-age
/// floor (verified against pnpm 10: an unfiltered importer on `^2.0.0` moved to the newest `2.x`,
/// not to the filtered importer's target, and one on `~2.0.0` stayed).
/// An excluded importer moving within its own range is therefore the resolver's doing, not a pin
/// cooldown placed — refusing it would let an excluded subtree veto every shared upgrade, the very
/// thing exclusions must not do — so it is `re_resolved`: committed and reported.
/// A changed declaration (`specifier:`), an entry appearing or disappearing, or a version its own
/// range provably excludes is `drifted`: pnpm never does that on its own, so it is a stale excluded
/// importer refreshed or an override that reached it, and it must not be committed as if the
/// exclusion had been honored.
/// The range judgment is proof-only, like [`MemberIndex::splits_for`]: a range cooldown cannot read
/// (a `||` union, a protocol) proves nothing, so the move counts as re-resolved rather than as drift.
/// `link:`/`workspace:`/`file:` entries are layout facts no resolve changes and are not compared;
/// one turning into or out of a registry version still shows as an added or removed entry.
#[derive(Default)]
struct ExcludedMoves {
    re_resolved: Vec<ExcludedMove>,
    drifted: Vec<ExcludedMove>,
}

fn excluded_importer_moves(
    before: &MemberIndex,
    after: &MemberIndex,
    excluded: &HashSet<String>,
) -> ExcludedMoves {
    let mut importers: Vec<&String> = excluded.iter().collect();
    importers.sort();
    let mut moves = ExcludedMoves::default();
    for importer in importers {
        let was = before.entries_of(importer);
        let now = after.entries_of(importer);
        let names: BTreeSet<&str> = was.keys().chain(now.keys()).copied().collect();
        for name in names {
            let record = |movement: String| ExcludedMove {
                importer: importer.clone(),
                name: name.to_string(),
                movement,
            };
            match (was.get(name), now.get(name)) {
                (Some(from), Some(to)) if from == to => {}
                (Some(from), Some(to)) if from.specifier == to.specifier => {
                    let excluded_by_range = to.specifier.as_deref().is_some_and(|specifier| {
                        crate::version::range_match(specifier, &to.version)
                            == crate::version::RangeMatch::Excludes
                    });
                    let movement = record(format!("{} → {}", from.version, to.version));
                    if excluded_by_range {
                        moves.drifted.push(movement);
                    } else {
                        moves.re_resolved.push(movement);
                    }
                }
                (Some(from), Some(to)) => moves.drifted.push(record(format!(
                    "declared {} → {}, {} → {}",
                    from.specifier.as_deref().unwrap_or("no specifier"),
                    to.specifier.as_deref().unwrap_or("no specifier"),
                    from.version,
                    to.version
                ))),
                (None, Some(to)) => moves
                    .drifted
                    .push(record(format!("added at {}", to.version))),
                (Some(from), None) => moves
                    .drifted
                    .push(record(format!("removed at {}", from.version))),
                (None, None) => {}
            }
        }
    }
    moves
}

/// Accounts for every excluded-importer entry the settled resolve changed
/// ([`excluded_importer_moves`]): a drifted entry refuses the batch — cooldown never re-resolves an
/// importer it was told to ignore, and the non-local rejection lets candidate isolation hold the
/// candidate whose landing did it — while re-resolved copies are committed under one warning each,
/// naming the importer and carrying the package, so the excluded subtree can neither veto the
/// upgrade nor move silently.
fn report_excluded_moves(
    report: &mut ApplyReport,
    before: &MemberIndex,
    after: &MemberIndex,
    excluded: &HashSet<String>,
) -> Result<()> {
    let moves = excluded_importer_moves(before, after, excluded);
    if !moves.drifted.is_empty() {
        return Err(CoreError::UnacceptableResolve(format!(
            "the resolve changed dependencies of importers the run excludes beyond re-resolving them within their own ranges: {}; cooldown never re-resolves an importer it was told to ignore",
            moves
                .drifted
                .iter()
                .map(ExcludedMove::describe)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    for moved in &moves.re_resolved {
        report.warnings.push(
            Diagnostic::new(
                DiagnosticKind::Held,
                format!(
                    "pnpm re-resolved {} to the newest version its own range admits, as its update does in every importer whatever the filter; the excluded manifest was not touched",
                    moved.describe()
                ),
            )
            .with_package(moved.name.clone()),
        );
    }
    Ok(())
}

/// The whole-lock guards on the settled resolve, before any candidate is reported applied: excluded
/// importers accounted for ([`report_excluded_moves`]), no in-scope importer split
/// ([`report_importer_splits`]), and every graph-level duplicate the resolve introduced named
/// ([`report_graph_duplicates`]).
/// All need the pre-apply lock as their baseline; without one there is nothing an importer could
/// have moved or split away from, so a lock the run created is taken as it is.
fn guard_settled_lock<L: NodeLock>(
    report: &mut ApplyReport,
    lock: (Option<&str>, &str),
    members: (Option<&MemberIndex>, &MemberIndex),
    excluded: &HashSet<String>,
) -> Result<()> {
    let (Some(before_content), Some(before_members)) = (lock.0, members.0) else {
        return Ok(());
    };
    report_excluded_moves(report, before_members, members.1, excluded)?;
    let accounted = report_importer_splits(report, before_members, members.1, excluded)?;
    report_graph_duplicates::<L>(report, before_content, lock.1, &accounted)
}

/// The importer paths `plan` excludes.
fn excluded_paths(plan: &Plan) -> HashSet<String> {
    plan.excluded_members
        .iter()
        .map(|member| member.path.clone())
        .collect()
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
        // A project without a journaled lock diffs from an empty baseline; a present but
        // unparsable one is a real error, never an empty graph.
        let before = journaled_lock::<L>(baseline_journal)
            .map(locked_versions::<L>)
            .transpose()?
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
        // This is fresh post-install content the driver just wrote, so the earlier strict reads
        // prove nothing about it: a failed parse must fail the batch rather than diff against an
        // empty map that hides every collateral move. Only an absent lock diffs as empty.
        let after = after_content
            .as_deref()
            .map(locked_versions::<L>)
            .transpose()?
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
        evidence: &PeerEvidence,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }
        let workspace = &evidence.workspace;
        let multi_version = &evidence.multi_version;

        // The pre-apply lock is captured in the journal.
        // Everything derived from the starting graph reads this one copy, so it all sees exactly
        // the lock the resolve starts from without another disk read.
        let before_content = journaled_lock::<L>(journal);
        // A `catalog:`-managed candidate can never land through the joint update: its version pin
        // lives in pnpm-workspace.yaml's catalog definition, which cooldown does not edit — the
        // manifest widen refuses protocol specifiers, and `pnpm update <name>@<target> --no-save`
        // re-resolves the importer back to the catalog pin. Left in the plan it would be reported
        // as a resolver conflict on every future run, so it is held up front with a truthful
        // not-eligible row instead. Transitive same-name candidates stay: the advance verdicts
        // below own them.
        // Judged with the run's excluded importers left out: their plain-range declarations are not
        // the run's to land, so they must not talk an included catalog-only candidate out of the
        // hold.
        let excluded = excluded_paths(plan);
        let catalog_managed = before_content
            .map(|content| L::catalog_managed_names(content, &excluded))
            .unwrap_or_default();
        let managed_index =
            before_content.map(|content| L::member_sources_excluding(content, &excluded));
        let (plan, catalog_holds) =
            partition_catalog_managed(plan, &catalog_managed, managed_index.as_ref());
        report.skipped.extend(catalog_holds);
        if plan.changes.is_empty() {
            // Nothing left to resolve; the catalog holds are the whole report.
            return Ok(report);
        }
        let plan = &plan;
        // An absent pre-apply lock diffs from an empty baseline; a present but unparsable one is
        // a real error, never an empty graph.
        let before = before_content
            .map(locked_versions::<L>)
            .transpose()?
            .unwrap_or_default();
        // Per-candidate transitive-advance verdicts against the pre-apply lock: which candidates
        // ride the qualified-override leg, and why each of the rest must be held (importer-owned
        // name, duplicated graph copies, no safe override scope).
        let importer_declared = managed_importer_declarations::<L>(before_content, &excluded);
        let version_lines = before_content
            .map(resolved_version_lines::<L>)
            .transpose()?
            .unwrap_or_default();
        let advance = classify_transitive_advance(plan, &importer_declared, &version_lines);

        // pnpm's `minimumReleaseAge` is a *rolling* age, so the cutoff is realized against the
        // current instant.
        // An absolute `--freeze` cutoff becomes `now - freeze` minutes — equivalent to the
        // freeze date as long as the same `now` governs both the seed and this resolve (it does:
        // wall-clock advances only seconds between them, far below the day-scale window under test).
        // It is passed only as the *transitive* floor here; each planned candidate is pinned to its
        // exact per-package target, so its own window is enforced by the pin rather than this cap.
        let window_minutes =
            window_minutes_from_cutoff(project.exclude_newer.as_deref(), jiff::Timestamp::now());

        let mut rejections: Vec<Skipped> = Vec::new();
        let after_content = self
            .resolve_and_verify(
                project,
                &JointResolve {
                    plan,
                    journal,
                    multi_version,
                    advance: &advance,
                    window_minutes,
                    workspace,
                },
                &mut rejections,
            )
            .await?;
        // Post-resolve content pnpm just wrote: a strict-parse failure fails the batch — an empty
        // default would report every candidate held and every collateral move invisible.
        let after = locked_versions::<L>(&after_content)?;
        // Per-importer resolved versions, so a candidate's landing is judged at *its* member rather
        // than the name's newest copy — the multi-version float leaves a lower line short of a
        // cross-line target the higher line already satisfies.
        let after_members = L::member_sources(&after_content);
        let before_members = before_content.map(L::member_sources);
        guard_settled_lock::<L>(
            &mut report,
            (before_content, &after_content),
            (before_members.as_ref(), &after_members),
            &excluded,
        )?;
        let before_members = before_members.unwrap_or_default();
        // The hold detail names the declarations that still disagree with the target — only the
        // in-scope ones, since an excluded importer's line never counted.
        let after_in_scope = L::member_sources_excluding(&after_content, &excluded);

        // A rejected candidate (peer-held, or rolled back after a partial landing) already carries
        // its structured skip row; the diff loop below must not add a second (resolver-conflict)
        // verdict for it.
        // Matched on the whole change, not on `(name, target)`: two lines of one name can share a
        // target (a split converging under `--rewrite`), and rejecting one must not hide the row of
        // the sibling that landed.
        let rejected = |change: &Change| rejections.iter().any(|skip| skip.change == *change);
        for change in &plan.changes {
            let name = change.package.name.as_str();
            if rejected(change) {
                continue;
            }
            let moved = candidate_moved(name, &before, &after, &before_members, &after_members);
            // A transitive candidate the advance pass held (importer-owned name, duplicated graph
            // copies, no override scope) was never handed to the resolver; its truthful hold row
            // replaces both the generic conflict verdict and — for a duplicate copy whose newest
            // line already sits at the target — the silent no-op the newest-copy projection would
            // otherwise produce.
            let advance_hold = advance
                .get(&advance_key(change))
                .and_then(|verdict| verdict.hold_skip(change));
            if reached(&after, &after_members, change) {
                if moved {
                    report.applied.push(change.clone());
                } else if let Some(hold) = advance_hold {
                    report.skipped.push(hold);
                }
                // Otherwise: reached its target without a net lock move because a duplicate copy
                // of the same name is at the target — a no-op, neither applied nor held.
            } else if let Some(hold) = advance_hold {
                report.skipped.push(hold);
            } else if multi_version.contains(name) {
                report
                    .skipped
                    .push(multi_version_hold(change, &after_in_scope, plan));
            } else {
                let advanced = matches!(
                    advance.get(&advance_key(change)),
                    Some(TransitiveAdvance::Pin(_))
                );
                report.skipped.push(resolver_conflict_hold::<L>(
                    change,
                    &after_content,
                    advanced,
                )?);
            }
        }

        report.skipped.extend(rejections);

        // The hard requirement is that no net version change to any package may be omitted.
        // Every moved package the applied rows above do not already report is surfaced as its own collateral
        // applied row — including a *held* candidate the resolve still floated off its baseline
        // (whose skip row alone would hide that real move).
        let collateral = collateral_changes::<L>(&before, &after, &report.applied);
        report.applied.extend(collateral);
        Ok(report)
    }

    /// Runs the joint resolve to a verified fixed point and returns the accepted lock content.
    ///
    /// Two things the resolver commits on its own are verified after every round.
    /// pnpm only *warns* on a peer mismatch, so the resolve can commit a graph that provably breaks
    /// a recorded contract between two importer-declared packages when one side of a pair is
    /// missing from the plan (a host held by a ceiling while its dependent moves —
    /// `react-dom@19(react@18)` requiring `react@^19`).
    /// And an exact pin is a request for every declaring importer, yet pnpm can land it in some and
    /// leave a peer-bound one behind (or, on the workspace-wide override path, reach an importer
    /// the plan excludes) — a partial landing that would split a single-copy name, or move an
    /// excluded copy, with nothing in the report saying so.
    ///
    /// Each round runs the resolve, then diffs the proven violations against the pre-apply baseline
    /// (gathered once — see [`PeerBaseline`]) and rejects every candidate a violation uniquely
    /// proves culpable ([`plan_peer_rejections`]) with structured `peer_held` blame, and rejects
    /// every pin that landed partially ([`partial_landings`]) with a resolver-conflict row naming
    /// the importers that did and did not take it; the journal is restored and the remainder
    /// re-resolved, so unrelated moves survive without the caller's bisect and extra rounds
    /// correspond only to real cascades.
    /// An unattributable violation propagates as a non-local rejection for candidate isolation.
    /// The rounds are bounded by the plan length (each continuing round removes at least one
    /// candidate); when every candidate is rejected, the restored pre-apply lock is the result.
    async fn resolve_and_verify(
        &self,
        project: &Project,
        inputs: &JointResolve<'_>,
        rejections: &mut Vec<Skipped>,
    ) -> Result<String> {
        let &JointResolve {
            plan,
            journal,
            multi_version,
            advance,
            window_minutes,
            workspace,
        } = inputs;
        let before_content = journaled_lock::<L>(journal);
        let baseline = PeerBaseline::gather::<L>(before_content, workspace);
        let mut active = plan.clone();
        loop {
            let resolve_result = self
                .whole_graph_resolve(project, &active, multi_version, advance, window_minutes)
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
                        advance,
                        window_minutes,
                    )
                    .await?;
                    resolve.postimage = journal.capture_state()?;
                }
                // The joint resolve is unsatisfiable as a whole.
                // Restore its partial work, then let `apply_resilient` isolate the offending
                // candidate (an unfetchable version or one side of a conflict) instead of holding
                // the complete batch.
                Err(error) => {
                    restore_after_owned_step(journal, &resolve.postimage)?;
                    return Err(error);
                }
            }

            let mut after_content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
            if new_lock_inconsistency::<L>(before_content, &after_content).is_some() {
                // pnpm's targeted `update` can float an *unrelated* importer entry out of its
                // declared range: peer unification across linked workspace members pulls a sibling
                // importer's version line into this one (observed on luup5 as `vite: ^6` landing
                // at 7.3.5 while updating `@playwright/test`). The override-based repair engine
                // resolves through a plain install, which provably respects declared ranges, so
                // the same pins are retried through it; only a repair that still leaves a fresh
                // inconsistency is a real stale-lock failure.
                restore_after_owned_step(journal, &resolve.postimage)?;
                self.repair_policy_rejected_graph(
                    project,
                    &active,
                    multi_version,
                    advance,
                    window_minutes,
                )
                .await?;
                resolve.postimage = journal.capture_state()?;
                after_content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
                if let Some(detail) = new_lock_inconsistency::<L>(before_content, &after_content) {
                    return Err(CoreError::StaleLock(detail));
                }
            }
            let current = proven_peer_violations::<L>(&after_content, workspace);
            let peer = plan_peer_rejections(&baseline, &current, &active, multi_version)?;
            let partial =
                partial_landings(&active, multi_version, &L::member_sources(&after_content));
            if peer.is_empty() && partial.is_empty() {
                return Ok(after_content);
            }
            // One verdict per candidate; a peer contract's blame is the more specific one when a
            // candidate earned both.
            let mut verdicts: BTreeMap<usize, Rejection> = partial
                .into_iter()
                .map(|landing| (landing.index, Rejection::Partial(landing)))
                .collect();
            verdicts.extend(
                peer.into_iter()
                    .map(|rejection| (rejection.index, Rejection::Peer(rejection))),
            );
            // Highest index first, so each removal leaves the remaining indices valid.
            for (index, verdict) in verdicts.into_iter().rev() {
                let change = active.changes.remove(index);
                rejections.push(match verdict {
                    Rejection::Peer(rejection) => {
                        peer_held_skip::<L>(&change, &rejection.violation, rejection.offending)
                    }
                    Rejection::Partial(landing) => {
                        partial_landing_skip::<L>(&change, &landing, &after_content)
                    }
                });
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
    /// Transitive candidates take a second leg: `pnpm update` selectors match direct dependencies
    /// only (`--depth` notwithstanding), so a package no importer declares is advanced through the
    /// temporary qualified-override engine instead — see
    /// [`Self::resolve_with_temporary_overrides`].
    async fn whole_graph_resolve(
        &self,
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
        advance: &HashMap<AdvanceKey, TransitiveAdvance>,
        window_minutes: Option<i64>,
    ) -> Result<()> {
        let inputs = Self::prepare_whole_graph_inputs(project, plan, multi_version)?;
        if !inputs.exact_pins.is_empty() {
            self.joint_resolve(
                project,
                &inputs.exact_pins,
                &inputs.importer_filters,
                window_minutes,
            )
            .await?;
            // The up-front pass already widened every out-of-range exact target, so a candidate the
            // resolve still left short of its target is blocked by *another* package's requirement
            // (a peer conflict), which widening its own declared range cannot resolve — the lock
            // diff reports it held.
            // No post-resolve re-widen loop is needed.
        }
        let transitive = transitive_override_pins(plan, advance);
        if !transitive.is_empty() {
            self.resolve_with_temporary_overrides(project, plan, transitive, window_minutes)
                .await?;
        }
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
            if !change.direct {
                // A transitive candidate has no importer declaration for `pnpm update` to match
                // (named selectors re-pin direct dependencies only), so it takes the
                // qualified-override leg instead of adding an unmatchable selector here — which,
                // via its empty member set, would also force the recursive fallback for the whole
                // batch.
                continue;
            }
            if multi_version.contains(&name) {
                // A held workspace split: preserve every distinct line.
                // A bare pnpm update can write an out-of-range lock entry while leaving
                // package.json untouched.
                // (Under `--rewrite` the split is not held — see `held_split_names` — and takes the
                // widen-and-pin path below like any other candidate.)
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
            // Two lines of one name converging on one target under `--rewrite` share a pin; pnpm
            // gets it once.
            let pin = (name, change.to.as_str().to_string());
            if !pins.contains(&pin) {
                pins.push(pin);
            }
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
        advance: &HashMap<AdvanceKey, TransitiveAdvance>,
        window_minutes: Option<i64>,
    ) -> Result<()> {
        // The repair replays the whole plan through the override engine, so the direct exact pins
        // and the qualified transitive pins ride one temporary config together.
        let inputs = Self::prepare_whole_graph_inputs(project, plan, multi_version)?;
        let mut pins = qualified_direct_override_pins(inputs.exact_pins);
        pins.extend(transitive_override_pins(plan, advance));
        if pins.is_empty() {
            return Ok(());
        }
        self.resolve_with_temporary_overrides(project, plan, pins, window_minutes)
            .await
    }

    /// Resolves the graph under temporary `pnpm-workspace.yaml` overrides for `pins`, then settles
    /// without them — the one pnpm mechanism that reaches a package no importer declares.
    ///
    /// The pins are merged over the configured overrides and written to the native config; one
    /// resolution-only install lands them; the original config is restored; and a settlement
    /// install re-validates the result override-free. The settlement is what makes the engine
    /// self-validating: pnpm keeps a lock entry that satisfies its dependents' declared ranges and
    /// re-resolves one that does not, so an in-range pin survives at exactly its target while an
    /// out-of-range force (an exact-pinning parent) reverts instead of committing a break — the
    /// caller's lock diff then reports that candidate held.
    async fn resolve_with_temporary_overrides(
        &self,
        project: &Project,
        plan: &Plan,
        pins: Vec<(String, String)>,
        window_minutes: Option<i64>,
    ) -> Result<()> {
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
        overrides.extend(pins);

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
        let restore_result = match native_snapshot.capture_state() {
            Ok(native_postimage) => restore_after_owned_step(&native_snapshot, &native_postimage),
            // With no readable postimage the unchanged-check is impossible, but the temporary
            // overrides just written must not leak into the user's config: fall back to the
            // identity-validated unconditional restore and surface the original failure. Leaving
            // our own write in place is strictly worse than restoring over it.
            Err(error) => {
                native_snapshot.restore()?;
                Err(error)
            }
        };
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

/// One transitive candidate's advance verdict, decided against the pre-apply lock.
///
/// The engine-level capability ([`cooldown_core::ToolWrite::supports_transitive_advance`]) is
/// unconditional, so every per-candidate limit must surface here as a truthful skip row instead of
/// silently narrowing the attempted set: a planned candidate the resolver was never asked to move
/// must not masquerade as a resolver conflict — or vanish behind the report's newest-copy
/// projection, whose per-name reduction cannot attribute a multi-copy collapse.
pub(crate) enum TransitiveAdvance {
    /// Advance through this qualified override key (`name@^major-line`).
    Pin(String),
    /// An importer declares the name (at another line — the same line would be one direct dep):
    /// the targeted update and its peer unification own the name, and a graph-wide override would
    /// drag the declared copy along with this one.
    DeclaredElsewhere,
    /// The name resolves to several graph copies; one qualified override could collapse them, a
    /// move the newest-copy report projection cannot attribute. Each copy is held on its own line,
    /// mirroring the importer-side multi-version hold.
    MultiLine(Vec<String>),
    /// No safe override scope can be derived from the current version, so there is nothing to
    /// qualify the pin with.
    Unscopable,
}

impl TransitiveAdvance {
    /// The truthful skip row for a non-[`Pin`](Self::Pin) verdict; `None` when the candidate was
    /// genuinely handed to the resolver.
    fn hold_skip(&self, change: &Change) -> Option<Skipped> {
        let (reason, detail) = match self {
            TransitiveAdvance::Pin(_) => return None,
            TransitiveAdvance::DeclaredElsewhere => (
                SkipReason::MultiVersionHeld,
                format!(
                    "importers declare {} at another line; this undeclared copy is kept on its own line",
                    change.package.name
                ),
            ),
            TransitiveAdvance::MultiLine(lines) => (
                SkipReason::MultiVersionHeld,
                format!(
                    "resolved at multiple versions across the graph ({}); each copy is kept on its own line",
                    lines.join(", ")
                ),
            ),
            TransitiveAdvance::Unscopable => (
                SkipReason::NotEligible,
                format!("no safe override scope can be derived from {}", change.from),
            ),
        };
        Some(Skipped {
            change: change.clone(),
            reason,
            offending: None,
            detail: Some(detail),
        })
    }
}

/// The per-candidate advance key: a change's `(name, from)` line. Two planned changes can share it
/// only by sharing the whole line, so the verdict is well-defined per key.
pub(crate) type AdvanceKey = (String, String);

/// The names the importers the run manages declare — the set a transitive-advance override must not
/// reach ([`TransitiveAdvance::DeclaredElsewhere`]).
/// Excluded importers' declarations are left out: they are not the run's to hold a transitive copy
/// for, and a qualified override that reaches one anyway fails the batch through
/// [`report_excluded_moves`] rather than being forestalled by a hold on every in-scope copy.
fn managed_importer_declarations<L: NodeLock>(
    content: Option<&str>,
    excluded: &HashSet<String>,
) -> HashSet<String> {
    content
        .map(|content| L::member_sources_excluding(content, excluded).declared_names())
        .unwrap_or_default()
}

fn advance_key(change: &Change) -> AdvanceKey {
    (
        change.package.name.clone(),
        change.from.as_str().to_string(),
    )
}

/// Classifies every transitive (non-direct) candidate against the pre-apply lock: which are
/// advanced through a qualified override, and why each of the rest is held. Computed once from the
/// full plan — peer-rejection rounds shrink the plan, and a verdict depends only on the pre-apply
/// lock, never on the surviving candidate set.
fn classify_transitive_advance(
    plan: &Plan,
    importer_declared: &HashSet<String>,
    version_lines: &HashMap<String, BTreeSet<String>>,
) -> HashMap<AdvanceKey, TransitiveAdvance> {
    let mut verdicts = HashMap::new();
    for change in plan.changes.iter().filter(|change| !change.direct) {
        let name = &change.package.name;
        let verdict = if importer_declared.contains(name) {
            TransitiveAdvance::DeclaredElsewhere
        } else if let Some(lines) = version_lines.get(name).filter(|lines| lines.len() > 1) {
            TransitiveAdvance::MultiLine(lines.iter().cloned().collect())
        } else {
            match major_line_qualifier(change.from.as_str()) {
                Some(qualifier) => TransitiveAdvance::Pin(format!("{name}@{qualifier}")),
                None => TransitiveAdvance::Unscopable,
            }
        };
        verdicts.insert(advance_key(change), verdict);
    }
    verdicts
}

/// The temporary override entries for the plan's advanceable transitive candidates: each
/// [`TransitiveAdvance::Pin`] verdict becomes a major-line-qualified override (`name@^3.0.0`,
/// `name@^0.11.0`) pinned to its exact matured target, so only the candidate's own version line is
/// addressed and a same-name copy on another major stays untouched.
fn transitive_override_pins(
    plan: &Plan,
    advance: &HashMap<AdvanceKey, TransitiveAdvance>,
) -> Vec<(String, String)> {
    let mut pins: Vec<(String, String)> = plan
        .changes
        .iter()
        .filter(|change| !change.direct)
        .filter_map(|change| {
            let TransitiveAdvance::Pin(key) = advance.get(&advance_key(change))? else {
                return None;
            };
            Some((key.clone(), change.to.as_str().to_string()))
        })
        .collect();
    pins.sort();
    pins.dedup();
    pins
}

/// Splits the plan's `catalog:`-managed **direct** candidates into truthful up-front holds,
/// returning the remaining plan and their skip rows.
///
/// Only direct candidates are held here: a transitive candidate that merely shares a
/// catalog-managed name is importer-declared, so the transitive-advance verdicts already hold it
/// on their own line ([`TransitiveAdvance::DeclaredElsewhere`]).
fn partition_catalog_managed(
    plan: &Plan,
    catalog_managed: &HashSet<String>,
    index: Option<&MemberIndex>,
) -> (Plan, Vec<Skipped>) {
    // A line whose every declaring importer uses a `catalog:` specifier is catalog-managed even
    // when a sibling line declares the name with a plain range: the sibling can still land, and
    // this line gets its truthful row instead of an eternal resolver conflict.
    let line_is_catalog = |change: &Change| {
        !change.members.is_empty()
            && index.is_some_and(|index| {
                change.members.iter().all(|member| {
                    index
                        .entries_of(&member.path)
                        .get(change.package.name.as_str())
                        .and_then(|entry| entry.specifier.as_deref())
                        .is_some_and(|specifier| specifier.starts_with("catalog:"))
                })
            })
    };
    let mut retained = Vec::with_capacity(plan.changes.len());
    let mut skips = Vec::new();
    for change in &plan.changes {
        let name = change.package.name.as_str();
        if change.direct && (catalog_managed.contains(name) || line_is_catalog(change)) {
            skips.push(catalog_managed_hold(change));
        } else {
            retained.push(change.clone());
        }
    }
    (
        Plan {
            changes: retained,
            ..plan.clone()
        },
        skips,
    )
}

/// The truthful hold for a candidate whose every declaring importer manages it through a pnpm
/// catalog: there is no editable version requirement for cooldown to retarget — the pin lives in
/// `pnpm-workspace.yaml`'s catalog definition, which cooldown does not edit — so the row says so
/// instead of masquerading as a resolver conflict.
fn catalog_managed_hold(change: &Change) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::NotEligible,
        offending: Some(change.package.clone()),
        detail: Some(format!(
            "{} is catalog-managed (a `catalog:` specifier); cooldown does not edit catalog \
             definitions — update the catalog entry in pnpm-workspace.yaml to move it",
            change.package.name
        )),
    }
}

/// The conservative hold for a name the joint resolve must not pin ([`held_split_names`]): it is
/// deliberately kept in range instead of pinned, and it must not be advertised as adoptable —
/// `outdated`'s verify reclassifies it blocked.
/// A name planned at several targets is held for that alone, and its row says so: only one target
/// per name — `--major` admitting the higher line — can settle it, so the row must not blame the
/// ranges, which may every one admit each target.
/// Otherwise the detail names the declared ranges, since the ranges are what must converge before
/// a joint pin becomes possible: a range that excludes the target, or one cooldown cannot judge (a
/// `||` union, a dist tag), is the hold's whole reason — never the ranges' mere disagreement, which
/// by itself holds nothing — and the row ends with `--rewrite`, which widens them.
/// The divergent resolved lines are named too when there are several.
fn multi_version_hold(change: &Change, after_members: &MemberIndex, plan: &Plan) -> Skipped {
    let name = change.package.name.as_str();
    let target = change.to.as_str();
    let versions = after_members.resolved_versions_of(name);
    let lines = if versions.len() > 1 {
        format!(
            "declared at multiple versions across the workspace ({})",
            versions.join(", ")
        )
    } else {
        "declared across the workspace".to_string()
    };
    let targets = planned_targets(plan).remove(name).unwrap_or_default();
    // Under `--rewrite` a split is pinned and widened, so a name still held is one the run plans at
    // several targets, and the row follows the rewrite mode rather than the count alone.
    // Should the retained plan ever show a single target for such a name, the list is left out
    // rather than shortened to a misleading one.
    let several_targets = targets.len() > 1 || plan.rewrite == RewriteMode::Always;
    let (reason, action) = if several_targets {
        let listed = if targets.len() > 1 {
            format!(" ({})", targets.into_iter().collect::<Vec<_>>().join(", "))
        } else {
            String::new()
        };
        (
            format!("planned at several targets{listed}, which one joint pin cannot land"),
            "admit one line for every importer (--major) before --rewrite can converge it"
                .to_string(),
        )
    } else {
        let specifiers = after_members.declared_specifiers_of(name);
        let ranges = if specifiers.is_empty() {
            format!("no plain range to judge against {target}")
        } else {
            format!(
                "not every declared range admits {target} ({})",
                specifiers.join(", ")
            )
        };
        (
            ranges,
            format!("pass --rewrite to widen every declaring range and converge on {target}"),
        )
    };
    Skipped {
        change: change.clone(),
        reason: SkipReason::MultiVersionHeld,
        offending: None,
        detail: Some(format!(
            "{lines} and {reason}; each importer is kept on its own line — {action}"
        )),
    }
}

/// The held verdict for a candidate the joint resolve left short of its target: a
/// mutually-exclusive peer won, so the row names the sibling whose peer choice excluded it (the
/// candidate itself absent a unique blocker). For an attempted transitive advance (`advanced`)
/// the generic message hides the real cause — whether the qualified override never matched (an
/// exact or sub-line dependent range) or the override-free settlement reverted the pin, the
/// target sits outside some dependent's declared range — so the detail says that instead.
fn resolver_conflict_hold<L: NodeLock>(
    change: &Change,
    after_content: &str,
    advanced: bool,
) -> Result<Skipped> {
    let name = change.package.name.as_str();
    let offender = peer_conflict_blocker(after_content, name).unwrap_or_else(|| name.to_string());
    let detail = if advanced {
        // The settlement may have re-resolved the copy to a different in-range version than
        // the pre-apply one; the row names where the graph actually settled, since a stale
        // `from` would contradict the collateral row that shows the real landing.
        // An unparsable settlement lock fails the verdict instead of silently naming the
        // pre-apply version; only a name genuinely absent from a parsable lock falls back.
        let settled = resolved_version_lines::<L>(after_content)?
            .remove(name)
            .and_then(|versions| {
                let line = version::major_key(change.from.as_str()).0;
                versions
                    .into_iter()
                    .find(|version| version::major_key(version).0 == line)
            })
            .unwrap_or_else(|| change.from.as_str().to_string());
        Some(format!(
            "the transitive advance did not land: a dependent's declared range holds it at {settled}"
        ))
    } else {
        None
    };
    Ok(Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(PackageId::new(L::ID, offender, Some(NPM.to_string()))),
        detail,
    })
}

/// Every registry-resolved version line per name in the lock — the multiset the per-candidate
/// advance verdicts are judged against, where the report's newest-copy projection would collapse
/// duplicate copies.
///
/// The strict parse failure propagates instead of defaulting to an empty multiset: an empty
/// default would silently judge every name single-line, letting an advance verdict pass on a
/// corrupted lock (see [`locked_versions`] for the same fail-closed contract).
fn resolved_version_lines<L: NodeLock>(content: &str) -> Result<HashMap<String, BTreeSet<String>>> {
    let mut lines: HashMap<String, BTreeSet<String>> = HashMap::new();
    for NameVersion { name, version, .. } in L::parse(content)? {
        if version.contains(':') {
            continue;
        }
        lines.entry(name).or_default().insert(version);
    }
    Ok(lines)
}

/// The plan's direct exact pins re-keyed for the override engine: each name gains its target's
/// major-line qualifier (`semver` pinned to `7.7.3` becomes `semver@^7.0.0`). An unqualified
/// `name: target` override captures *every* same-name request in the graph, so an undeclared copy
/// on another major would be silently collapsed onto the direct target — and the newest-copy
/// report projection would never show it. Qualified to the target's line, the override addresses
/// only requests the plan actually steers (after widening, the declaring importer's range lives on
/// that line) while a foreign line keeps its own resolution. A target with no parsable major keeps
/// the unqualified key: better a broad pin than a repair that cannot steer its own candidate.
fn qualified_direct_override_pins(exact_pins: Vec<(String, String)>) -> Vec<(String, String)> {
    exact_pins
        .into_iter()
        .map(|(name, target)| {
            let key = match major_line_qualifier(&target) {
                Some(qualifier) => format!("{name}@{qualifier}"),
                None => name,
            };
            (key, target)
        })
        .collect()
}

/// The caret range covering `version`'s own major line (`3.4.8` → `^3.0.0`, `0.11.14` → `^0.11.0`)
/// — the qualifier that scopes a transitive override to the line being advanced. `None` when the
/// major cannot be parsed: with nothing safe to scope the override to, the candidate is left
/// unpinned rather than force-pinned graph-wide.
fn major_line_qualifier(version: &str) -> Option<String> {
    let key = version::major_key(version).0;
    if key.is_empty() {
        return None;
    }
    Some(if key.contains('.') {
        format!("^{key}.0")
    } else {
        format!("^{key}.0.0")
    })
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
        .filter(|violation| !targeted.contains(violation.package.name.as_str()))
    {
        exact_versions
            .entry(violation.package.name.clone())
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

/// The planned names the whole-graph resolve must NOT exact-pin: those whose workspace declarations
/// split under the planned target ([`MemberIndex::splits_for`]) — exact-pinning would drag some
/// importer off its own declared range — unlike everything else, which is exact-pinned.
/// A name with several planned targets splits when any of them does.
///
/// `--rewrite` ([`RewriteMode::Always`]) is the explicit opt-in to rewriting declared constraints,
/// which is exactly what converging a split needs, so under it a split name is pinned after all and
/// every declaring member's range widened to admit the target (`prepare_whole_graph_inputs`) — a
/// half-finished migration (`^2.6.0` beside `^3.6.0`) converges on one line instead of being
/// preserved forever.
/// A name planned at *several* targets (a `^22` line and a `^25` line each advancing within itself
/// when `--major` is off) is held whatever its ranges say and under either rewrite mode: one joint
/// `pnpm update` cannot pin one name to two versions.
/// Judged by the ranges alone, a single permissive range (`>=22`) admitting both targets would put
/// two pins of one name into one update, land a version nobody planned, and report the planned one.
/// Its row says what would converge it.
///
/// Judged over the pre-apply `content` with the plan's excluded importers left out
/// ([`NodeLock::member_sources_excluding`]): an importer the run excludes is not cooldown's to
/// move, so its declaration cannot veto an update in an included one.
/// Derived from per-importer declarations, NOT the full resolved package set: a direct dependency
/// that merely shares a name with a transitive copy resolved at another version is single-declared,
/// so it stays exact-pinned — its per-package window and any out-of-range widen are honored.
/// Counting the whole resolved graph instead would misclassify such a dep and float it, dropping
/// the widen so a cross-major/out-of-range target can never land.
/// A missing lock has no declarations, so nothing splits by range; a name planned at several
/// targets is held all the same, since that hold needs no lock to be true.
pub(crate) fn held_split_names<L: NodeLock>(content: Option<&str>, plan: &Plan) -> HashSet<String> {
    let index = content.map(|content| L::member_sources_excluding(content, &excluded_paths(plan)));
    let targets = planned_targets(plan);
    plan.changes
        .iter()
        .filter(|change| {
            let name = change.package.name.as_str();
            let several_targets = targets.get(name).is_some_and(|targets| targets.len() > 1);
            several_targets
                || (plan.rewrite == RewriteMode::Auto
                    && index
                        .as_ref()
                        .is_some_and(|index| index.splits_for(name, change.to.as_str())))
        })
        .map(|change| change.package.name.clone())
        .collect()
}

/// Every distinct target the plan names per package, ascending — several when the lines of a split
/// name each advance within themselves.
fn planned_targets(plan: &Plan) -> HashMap<&str, BTreeSet<&str>> {
    let mut targets: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for change in &plan.changes {
        targets
            .entry(change.package.name.as_str())
            .or_default()
            .insert(change.to.as_str());
    }
    targets
}

/// Why the verified resolve loop drops one active candidate for the next round.
enum Rejection {
    /// A proven peer contract uniquely blames it.
    Peer(crate::peers::PeerRejection),
    /// Its pin landed in only some of its importers.
    Partial(PartialLanding),
}

#[async_trait]
impl<L: NodeLock> ToolWrite for NpmTool<L> {
    fn mutation_tool(&self) -> ToolId {
        L::ID
    }

    fn supports_transitive_advance(&self) -> bool {
        L::SUPPORTS_TRANSITIVE_ADVANCE
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
        let evidence = PeerEvidence::gather::<L>(Some(&project.root), lock, plan);
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
            self.apply_whole_graph(project, &plan, journal, &evidence)
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
        // A refresh the package manager rejects is a tool failure, not a stale lock: it proves
        // nothing about the lock, so it must not be waved through with `--allow-stale-lock`.
        self.cmd.run(&project.root, &args).await?;
        Ok(Some(LockVerifyReport {
            status: LockStatus::Current,
            detail: format!("{} refreshed", L::LOCKFILE),
        }))
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
    use crate::lock::{Bun, Npm, Pnpm, Yarn};
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use indoc::indoc;

    #[test]
    fn advisory_ecosystem_matches_osv() {
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        for ecosystem in [
            NpmTool::<Npm>::from_http(http.clone())
                .capabilities()
                .advisory_ecosystem,
            NpmTool::<Pnpm>::from_http(http.clone())
                .capabilities()
                .advisory_ecosystem,
            NpmTool::<Yarn>::from_http(http.clone())
                .capabilities()
                .advisory_ecosystem,
            NpmTool::<Bun>::from_http(http)
                .capabilities()
                .advisory_ecosystem,
        ] {
            assert_eq!(ecosystem, Some("npm"));
        }
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
                    package: PackageId::new(ToolId(NPM), "eslint", None),
                    version: Version::new("10.7.0"),
                },
                cooldown_core::BaselineViolation {
                    package: PackageId::new(ToolId(NPM), "flatted", None),
                    version: Version::new("3.4.3"),
                },
                cooldown_core::BaselineViolation {
                    package: PackageId::new(ToolId(NPM), "flatted", None),
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
        let versions = locked_versions::<Pnpm>(lock).expect("a well-formed lock parses");
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
        let after_newest = locked_versions::<Pnpm>(lock).expect("a well-formed lock parses");
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

    /// Advisory identity needs the lock entry's own `resolved` URL to name the public registry:
    /// a private-registry URL or a missing record grants nothing. Only the decline direction is
    /// asserted here — it holds whatever npm configuration the host machine carries, since
    /// ambient config can only veto further, never grant. The grant chain is pinned by the
    /// hermetic `npmrc::advisory_identity` tests.
    #[tokio::test]
    async fn advisory_identity_requires_a_public_resolved_url() {
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
                    "node_modules/lodash": { "version": "4.17.15", "resolved": "https://npm.corp.example/lodash/-/lodash-4.17.15.tgz" },
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

        let graph = tool()
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph deps");
        let identity_of = |name: &str| {
            graph
                .iter()
                .find(|dep| dep.package.name == name)
                .expect("dep")
                .advisory_identity
                .clone()
        };
        assert_eq!(
            identity_of("lodash"),
            None,
            "a private-registry resolved URL proves the wrong origin"
        );
        assert_eq!(
            identity_of("ms"),
            None,
            "an entry without a resolved record carries no origin evidence"
        );
    }

    /// A dependency as `dependencies()` would grant it: identity present, everything else
    /// minimal — the confirmation hook's input. Unix-only because every consumer fakes the
    /// manager binary with a script, which needs unix permission bits.
    #[cfg(unix)]
    fn granted_dep(name: &str) -> Dependency {
        Dependency {
            package: PackageId::new(Npm::ID, name.to_string(), Some(NPM.to_string())),
            advisory_identity: Some(name.to_string()),
            current: cooldown_core::Version::new("1.0.0".to_string()),
            current_quality: cooldown_core::ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn effective_config_script(root: &Utf8Path, body: &str) -> Utf8PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let script = root.join("fake-npm.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        script
    }

    /// Confirmation asks the manager binary for its *effective* configuration — the merge of
    /// layers (global, builtin) no file walk can locate — and withholds exactly the identities
    /// it reroutes; a query that fails withholds them all, since unknown routing must not pass
    /// as none.
    #[cfg(unix)]
    #[tokio::test]
    async fn confirmation_withholds_identities_the_effective_registry_reroutes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let script = effective_config_script(
            &root,
            r#"echo '{"registry":"https://registry.npmjs.org/","@corp:registry":"https://npm.corp.example/"}'"#,
        );
        let mut npm = tool();
        npm.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut deps = vec![granted_dep("lodash"), granted_dep("@corp/api")];

        npm.confirm_advisory_identities(&project, &mut deps).await;
        assert_eq!(deps[0].advisory_identity.as_deref(), Some("lodash"));
        assert_eq!(
            deps[1].advisory_identity, None,
            "the effective scope routing vetoes what the lock granted"
        );

        // A failing query is asked once (memoized) and withholds everything.
        let script = effective_config_script(&root, "exit 1");
        let mut failing = tool();
        failing.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        let mut deps = vec![granted_dep("lodash")];
        failing
            .confirm_advisory_identities(&project, &mut deps)
            .await;
        assert_eq!(
            deps[0].advisory_identity, None,
            "unknown routing must not pass as none"
        );
    }

    /// yarn classic offers no reliable effective-config query and merges `.yarnrc` files up the
    /// directory tree, so its identities never survive confirmation — without spawning anything.
    #[cfg(unix)]
    #[tokio::test]
    async fn yarn_identities_never_survive_confirmation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let cache = tempfile::tempdir().expect("cache");
        let mut tool: NpmTool<crate::lock::Yarn> = NpmTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );
        // A binary that must never run: confirmation for yarn decides without asking.
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(root.join("absent-yarn").as_str());
        let project = Project {
            root: root.clone(),
            kind: crate::lock::Yarn::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut deps = vec![granted_dep("lodash")];

        tool.confirm_advisory_identities(&project, &mut deps).await;
        assert_eq!(deps[0].advisory_identity, None);
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

    /// A direct catalog-managed candidate is held up front with a truthful not-eligible row
    /// (never a resolver conflict), while a transitive same-name candidate stays in the plan for
    /// the advance verdicts and unrelated candidates keep the resolve path.
    #[test]
    fn catalog_managed_candidates_are_held_not_eligible_with_catalog_detail() {
        let mut transitive_copy = change("react", "17.0.2", "17.0.3");
        transitive_copy.direct = false;
        let plan = Plan {
            changes: vec![
                change("react", "18.3.1", "19.0.0"),
                change("lodash", "4.17.20", "4.17.21"),
                transitive_copy,
            ],
            ..Plan::default()
        };
        let catalog_managed = HashSet::from(["react".to_string()]);

        let (retained, skips) = partition_catalog_managed(&plan, &catalog_managed, None);

        let retained_names: Vec<(&str, bool)> = retained
            .changes
            .iter()
            .map(|change| (change.package.name.as_str(), change.direct))
            .collect();
        assert_eq!(
            retained_names,
            vec![("lodash", true), ("react", false)],
            "only the direct catalog copy is held"
        );
        assert_eq!(skips.len(), 1);
        let skip = &skips[0];
        assert_eq!(skip.reason, SkipReason::NotEligible);
        assert_eq!(skip.change.package.name, "react");
        assert_eq!(skip.change.to.as_str(), "19.0.0");
        let detail = skip.detail.as_deref().expect("a non-empty detail");
        assert!(!detail.is_empty());
        assert!(detail.contains("catalog-managed"), "{detail}");
        assert!(detail.contains("pnpm-workspace.yaml"), "{detail}");
    }

    /// The advanced-hold detail names where the settlement actually left the copy — it may have
    /// re-resolved to a different in-range version than the pre-apply one, and a stale `from`
    /// would contradict the collateral row that shows the real landing.
    #[test]
    fn advanced_hold_detail_names_the_settled_version_not_the_stale_from() {
        let mut advance = change("debug", "3.4.8", "3.5.0");
        advance.direct = false;
        let after = indoc::indoc! {"
            importers:

              .:
                dependencies:
                  consumer:
                    specifier: ^1.0.0
                    version: 1.0.0

            packages:

              debug@3.4.9:
                resolution: {integrity: sha512-a}
        "};

        let hold = resolver_conflict_hold::<Pnpm>(&advance, after, true)
            .expect("a parsable settlement lock yields the hold");
        let detail = hold.detail.expect("advanced holds carry a detail");
        assert!(
            detail.contains("holds it at 3.4.9"),
            "the settled copy, not the stale pre-apply version, is named: {detail}"
        );

        // An empty document is a legitimately empty lock, not a parse failure: the name is simply
        // absent, so the pre-apply version is the honest fallback.
        let unresolvable =
            resolver_conflict_hold::<Pnpm>(&advance, "", true).expect("an empty lock is parsable");
        let detail = unresolvable.detail.expect("advanced holds carry a detail");
        assert!(
            detail.contains("holds it at 3.4.8"),
            "with no settled copy to read, the pre-apply version is the honest fallback: {detail}"
        );
    }

    /// A present but unparsable settlement lock fails the advanced-hold verdict instead of
    /// silently falling back to the single-line/pre-apply reading — the same fail-closed contract
    /// as the resolved-package parse.
    #[test]
    fn advanced_hold_verdict_fails_closed_on_an_unparsable_lock() {
        let mut advance = change("debug", "3.4.8", "3.5.0");
        advance.direct = false;
        let malformed = indoc! {"
            lockfileVersion: '9.0'
            packages:
              debug@3.4.9: [unclosed
        "};

        let error = resolver_conflict_hold::<Pnpm>(&advance, malformed, true)
            .expect_err("an unparsable lock is an error, not a stale-from fallback");
        assert!(
            matches!(error, CoreError::LockUnreadable(_)),
            "typed lock error, got: {error:?}"
        );
    }

    #[test]
    fn transitive_advance_pins_the_major_line_and_holds_every_ineligible_candidate() {
        let mut deep = change("dompurify", "3.4.8", "3.4.12");
        deep.direct = false;
        let mut zero_major = change("proto-lite", "0.11.14", "0.11.16");
        zero_major.direct = false;
        // Declared by an importer (a direct `react@19` beside this transitive copy): the
        // targeted-update path owns the name, and a graph-wide override would drag the declared
        // copy — held, with a row saying so.
        let mut declared_elsewhere = change("react", "18.2.0", "18.3.1");
        declared_elsewhere.direct = false;
        // Two resolved copies of one undeclared name: a single qualified override could collapse
        // them, which the newest-copy report projection cannot attribute — held on its own lines.
        let mut duplicated = change("entities", "4.5.0", "4.5.3");
        duplicated.direct = false;
        // No parsable major line: nothing safe to scope an override to.
        let mut unscopable = change("weird", "not-semver", "1.0.0");
        unscopable.direct = false;
        let direct = change("chalk", "5.0.0", "5.3.0");
        let plan = Plan {
            changes: vec![
                deep,
                zero_major,
                declared_elsewhere.clone(),
                duplicated.clone(),
                unscopable.clone(),
                direct,
            ],
            ..Plan::default()
        };
        let importer_declared: HashSet<String> = HashSet::from(["react".to_string()]);
        let version_lines: HashMap<String, BTreeSet<String>> = HashMap::from([(
            "entities".to_string(),
            BTreeSet::from(["4.5.0".to_string(), "5.0.2".to_string()]),
        )]);

        let advance = classify_transitive_advance(&plan, &importer_declared, &version_lines);
        let pins = transitive_override_pins(&plan, &advance);

        assert_eq!(
            pins,
            vec![
                ("dompurify@^3.0.0".to_string(), "3.4.12".to_string()),
                ("proto-lite@^0.11.0".to_string(), "0.11.16".to_string()),
            ],
            "only eligible non-direct candidates are pinned, each on its own major line"
        );
        // Every ineligible candidate carries a truthful hold row — never a generic resolver
        // conflict for a move the resolver was not asked to make.
        let hold = |change: &Change| {
            advance
                .get(&advance_key(change))
                .and_then(|verdict| verdict.hold_skip(change))
                .expect("ineligible candidate holds")
        };
        let declared_hold = hold(&declared_elsewhere);
        assert_eq!(declared_hold.reason, SkipReason::MultiVersionHeld);
        assert!(
            declared_hold
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("importers declare react")),
        );
        let duplicated_hold = hold(&duplicated);
        assert_eq!(duplicated_hold.reason, SkipReason::MultiVersionHeld);
        assert!(
            duplicated_hold
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("4.5.0, 5.0.2")),
        );
        let unscopable_hold = hold(&unscopable);
        assert_eq!(unscopable_hold.reason, SkipReason::NotEligible);
        // A pinned candidate has no hold row: it was genuinely handed to the resolver.
        let pinned_key = ("dompurify".to_string(), "3.4.8".to_string());
        assert!(matches!(
            advance.get(&pinned_key),
            Some(TransitiveAdvance::Pin(_))
        ));
    }

    #[test]
    fn prepare_whole_graph_inputs_routes_transitives_off_the_targeted_update() -> eyre::Result<()> {
        // A transitive candidate must neither become a `pnpm update` selector (named selectors
        // match direct deps only — a no-op pin reported held) nor, via its empty member set,
        // force the whole batch onto the recursive fallback that runs in unrelated importers.
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "chalk": "^5.0.0" } }"#,
        )?;
        let project = Project {
            root: root.clone(),
            kind: Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut direct = change("chalk", "5.0.0", "5.3.0");
        direct.members = vec![cooldown_core::MemberRef {
            name: "root".to_string(),
            path: ".".to_string(),
        }];
        let mut transitive = change("dompurify", "3.4.8", "3.4.12");
        transitive.direct = false;
        let plan = Plan {
            changes: vec![direct, transitive],
            ..Plan::default()
        };

        let inputs = NpmTool::<Pnpm>::prepare_whole_graph_inputs(&project, &plan, &HashSet::new())?;

        assert_eq!(
            inputs.exact_pins,
            vec![("chalk".to_string(), "5.3.0".to_string())],
            "only the direct candidate is a targeted-update pin"
        );
        assert!(
            !inputs.importer_filters.is_empty(),
            "the transitive's empty member set must not force the recursive fallback"
        );
        Ok(())
    }

    /// An importer copy that genuinely moved beneath a newer transitive duplicate shows no net
    /// newest-copy change; the importer-resolved version sets keep its applied row visible, while
    /// a converged re-run (nothing moved anywhere) still reports no movement.
    #[test]
    fn a_move_under_a_newer_duplicate_copy_is_still_a_move() {
        let before_lock = indoc::indoc! {"
            importers:

              .:
                dependencies:
                  chalk:
                    specifier: ^4.0.0
                    version: 4.1.1

            packages:

              chalk@4.1.1:
                resolution: {integrity: sha512-a}
              chalk@5.6.0:
                resolution: {integrity: sha512-b}
        "};
        let after_lock = before_lock.replace("4.1.1", "4.1.2");
        let before = locked_versions::<Pnpm>(before_lock).expect("a well-formed lock parses");
        let after = locked_versions::<Pnpm>(&after_lock).expect("a well-formed lock parses");
        assert_eq!(
            before.get("chalk").map(String::as_str),
            Some("5.6.0"),
            "the newest-copy projection is blind to the importer copy"
        );

        let before_members = Pnpm::member_sources(before_lock);
        let after_members = Pnpm::member_sources(&after_lock);
        assert!(
            candidate_moved("chalk", &before, &after, &before_members, &after_members),
            "the importer copy moved 4.1.1 -> 4.1.2 beneath the 5.6.0 duplicate"
        );
        assert!(
            !candidate_moved("chalk", &before, &before, &before_members, &before_members),
            "a converged re-run reports no movement"
        );
    }

    /// npm's declaration attribution is name-only, so a landed root copy must be judged by the
    /// member's physical install-tree instance: a newer duplicate nested under another dependent
    /// otherwise masks the landing, and the per-package apply would roll it back as a resolver
    /// conflict on every run.
    #[test]
    fn npm_landing_is_judged_by_the_members_install_instance_not_the_newest_copy()
    -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("package-lock.json"),
            indoc::indoc! {r#"{
                "name": "root",
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/chalk": { "version": "4.1.2" },
                    "node_modules/x": { "version": "1.0.0" },
                    "node_modules/x/node_modules/chalk": { "version": "5.6.0" }
                }
            }"#},
        )?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut landed = change("chalk", "4.1.1", "4.1.2");
        landed.members = vec![cooldown_core::MemberRef {
            name: "root".to_string(),
            path: ".".to_string(),
        }];

        assert!(
            exact_target_reached::<Npm>(&project, &landed)?,
            "the root instance sits at the target; the nested 5.6.0 duplicate must not mask it"
        );
        let mut short = change("chalk", "4.1.1", "4.1.3");
        short.members.clone_from(&landed.members);
        assert!(
            !exact_target_reached::<Npm>(&project, &short)?,
            "an instance short of its target is still short"
        );
        Ok(())
    }

    /// A present but unparsable post-install lock fails the landing read-back instead of
    /// answering "unreached" — `Ok(false)` would roll a possibly-landed candidate back as a
    /// resolver conflict on every run.
    #[test]
    fn exact_target_reached_fails_closed_on_an_unparsable_lock() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        // Truncated JSON: present content the strict parse must reject.
        std::fs::write(root.join("package-lock.json"), r#"{ "lockfileVersion": 3,"#)?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let landed = change("chalk", "4.1.1", "4.1.2");

        let error = exact_target_reached::<Npm>(&project, &landed)
            .expect_err("an unparsable lock is an error, not an unreached target");
        assert!(
            matches!(error, CoreError::Parse(_)),
            "npm's strict lock parse error propagates, got: {error:?}"
        );
        Ok(())
    }

    /// Writes the floated-lock fixture: a manifest declaring `dep: ^1.0.0`, a consistent seed
    /// lock, the floated and repaired lock shapes, and a scripted fake pnpm whose `update` leg
    /// floats the importer entry out of its declared range (the shape pnpm was observed to
    /// produce under `--no-save`) while the override-engine legs repair it. Returns the script
    /// path to inject via [`crate::nodecmd::NodeCmd::with_bin`].
    #[cfg(unix)]
    fn write_floated_lock_fixture(root: &Utf8Path) -> eyre::Result<Utf8PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "app", "dependencies": { "dep": "^1.0.0" } }"#,
        )?;
        let lock = |version: &str| {
            indoc::formatdoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      dep:
                        specifier: ^1.0.0
                        version: {version}

                packages:

                  dep@{version}:
                    resolution: {{integrity: sha512-a}}
            "}
        };
        std::fs::write(root.join("pnpm-lock.yaml"), lock("1.0.0"))?;
        std::fs::write(root.join("floated.yaml"), lock("2.0.0"))?;
        std::fs::write(root.join("repaired.yaml"), lock("1.1.0"))?;
        let script = root.join("fake-pnpm.sh");
        std::fs::write(
            &script,
            indoc::indoc! {r#"
                #!/bin/sh
                case "$*" in
                  *"config get"*)
                    echo 'null'; exit 0 ;;
                  *" update "*)
                    echo update >> legs.log
                    cp floated.yaml pnpm-lock.yaml; exit 0 ;;
                  *"--resolution-only"*)
                    echo repair >> legs.log
                    cp pnpm-workspace.yaml overrides-at-repair.yaml
                    cp repaired.yaml pnpm-lock.yaml; exit 0 ;;
                  *"install"*)
                    if [ -f pnpm-workspace.yaml ]; then echo settle-with-overrides >> legs.log; else echo settle >> legs.log; fi
                    cp repaired.yaml pnpm-lock.yaml; exit 0 ;;
                esac
                echo "unexpected args: $*" >&2; exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(script)
    }

    /// The floated-lock retry end to end: pnpm's targeted `update` floats an importer entry out
    /// of its declared range, the inconsistency check restores the journal, the same pins are
    /// retried through the override engine (major-line-qualified, written to a temporary
    /// `pnpm-workspace.yaml` that must not persist), and the repaired lock reports the candidate
    /// applied instead of declaring the lock stale.
    #[cfg(unix)]
    #[tokio::test]
    async fn floated_lock_is_repaired_through_the_override_engine() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let script = write_floated_lock_fixture(&root)?;
        let cache = tempfile::tempdir()?;
        let mut tool = NpmTool::<Pnpm>::from_http(cooldown_registry::SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        let project = Project {
            root: root.clone(),
            kind: Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut planned = change("dep", "1.0.0", "1.1.0");
        planned.members = vec![cooldown_core::MemberRef {
            name: "app".to_string(),
            path: ".".to_string(),
        }];
        let plan = Plan {
            changes: vec![planned],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        let evidence =
            PeerEvidence::gather::<Pnpm>(Some(&root), journaled_lock::<Pnpm>(&journal), &plan);
        let report = tool
            .apply_whole_graph(&project, &plan, &journal, &evidence)
            .await?;

        assert_eq!(
            report
                .applied
                .iter()
                .map(|change| change.package.name.as_str())
                .collect::<Vec<_>>(),
            vec!["dep"],
            "the retried pins land through the override engine"
        );
        assert!(
            report.skipped.is_empty(),
            "nothing is held: {:?}",
            report.skipped
        );
        let legs = std::fs::read_to_string(root.join("legs.log"))?;
        assert_eq!(
            legs.lines().collect::<Vec<_>>(),
            vec!["update", "repair", "settle"],
            "float, override repair, then an override-free settlement"
        );
        let overrides = std::fs::read_to_string(root.join("overrides-at-repair.yaml"))?;
        assert!(
            overrides.contains("dep@^1.0.0") && overrides.contains("1.1.0"),
            "the repair rides the major-line-qualified pin: {overrides}"
        );
        assert!(
            !root.join("pnpm-workspace.yaml").exists(),
            "the temporary overrides file must not persist"
        );
        assert!(
            std::fs::read_to_string(root.join("pnpm-lock.yaml"))?.contains("dep@1.1.0"),
            "the repaired lock is the committed result"
        );
        Ok(())
    }

    /// The whole-graph report diff fails the batch when the resolve exits 0 but leaves a present,
    /// unparsable lock: the strict parse error propagates instead of diffing every candidate as
    /// held against an empty version map — the healthy-looking failure the fail-closed parse
    /// closes.
    #[cfg(unix)]
    #[tokio::test]
    async fn whole_graph_apply_fails_closed_when_the_resolve_writes_an_unparsable_lock()
    -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "app", "dependencies": { "dep": "^1.0.0" } }"#,
        )?;
        // The pre-apply lock is valid, so only the post-resolve re-read can fail the parse.
        std::fs::write(
            root.join("pnpm-lock.yaml"),
            indoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      dep:
                        specifier: ^1.0.0
                        version: 1.0.0

                packages:

                  dep@1.0.0:
                    resolution: {integrity: sha512-a}
            "},
        )?;
        std::fs::write(
            root.join("malformed.yaml"),
            indoc! {"
                lockfileVersion: '9.0'
                packages:
                  dep@1.1.0: [unclosed
            "},
        )?;
        // A scripted pnpm whose every resolve leg "succeeds" while leaving the unparsable lock.
        let script = root.join("fake-pnpm.sh");
        std::fs::write(
            &script,
            indoc! {r#"
                #!/bin/sh
                case "$*" in
                  *"config get"*)
                    echo 'null'; exit 0 ;;
                  *)
                    cp malformed.yaml pnpm-lock.yaml; exit 0 ;;
                esac
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let cache = tempfile::tempdir()?;
        let mut tool = NpmTool::<Pnpm>::from_http(cooldown_registry::SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        let project = Project {
            root: root.clone(),
            kind: Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let mut planned = change("dep", "1.0.0", "1.1.0");
        planned.members = vec![cooldown_core::MemberRef {
            name: "app".to_string(),
            path: ".".to_string(),
        }];
        let plan = Plan {
            changes: vec![planned],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        let evidence =
            PeerEvidence::gather::<Pnpm>(Some(&root), journaled_lock::<Pnpm>(&journal), &plan);
        let error = tool
            .apply_whole_graph(&project, &plan, &journal, &evidence)
            .await
            .expect_err("an unparsable post-resolve lock must fail the batch, not read as held");
        assert!(
            matches!(error, CoreError::LockUnreadable(_)),
            "typed lock error, got: {error:?}"
        );
        Ok(())
    }

    /// The repair's override keys must scope each direct pin to its target's major line so a
    /// same-name copy on another major is never captured; only an unparsable major falls back to
    /// the graph-wide key.
    #[test]
    fn repair_override_pins_are_qualified_to_the_target_major_line() {
        let pins = vec![
            ("semver".to_string(), "7.7.3".to_string()),
            ("tiny".to_string(), "0.11.14".to_string()),
            ("weird".to_string(), "not-a-version".to_string()),
        ];
        assert_eq!(
            qualified_direct_override_pins(pins),
            vec![
                ("semver@^7.0.0".to_string(), "7.7.3".to_string()),
                ("tiny@^0.11.0".to_string(), "0.11.14".to_string()),
                ("weird".to_string(), "not-a-version".to_string()),
            ]
        );
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

    /// The per-package post-apply diff fails the batch when the on-disk lock turns unparsable
    /// during the apply loop: the journaled BEFORE parsed fine, but the AFTER is new content whose
    /// failed parse must surface instead of diffing as an empty graph that hides every move.
    #[tokio::test]
    async fn apply_fails_closed_when_the_post_apply_lock_is_unparsable() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("package.json"), r#"{ "name": "root" }"#)?;
        std::fs::write(
            root.join("package-lock.json"),
            indoc! {r#"{
                "name": "root",
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "root" },
                    "node_modules/nanoid": { "version": "3.1.0" }
                }
            }"#},
        )?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        // No manifest declares nanoid, so the candidate is held not-eligible without invoking
        // npm and the batch still reaches the final before/after diff.
        let plan = Plan {
            changes: vec![change("nanoid", "3.1.0", "3.3.0")],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };
        let tool = tool();
        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        // Corrupt the on-disk lock after the journal captured the valid pre-apply copy — the
        // shape of a driver run that exits 0 but writes a lock the strict parse rejects.
        std::fs::write(root.join("package-lock.json"), r#"{ "lockfileVersion": 3,"#)?;

        let error = tool
            .apply(&mutation)
            .await
            .expect_err("an unparsable post-apply lock must fail the batch, not diff as empty");
        assert!(
            matches!(error, CoreError::Parse(_)),
            "npm's strict lock parse error propagates, got: {error:?}"
        );
        Ok(())
    }
}

#[cfg(test)]
#[cfg(unix)]
mod whole_graph_tests {
    //! The pnpm whole-graph apply against a scripted `pnpm`: what the joint resolve lands, rolls
    //! back, holds, and reports, judged per importer.
    //! Unix-only because the fake manager binary is a shell script that needs unix permission bits.

    use super::*;
    use crate::lock::Pnpm;
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use indoc::{formatdoc, indoc};
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    /// One importer of a scripted workspace: its path, package name, and the dependencies it
    /// declares as `(name, specifier, version)`.
    struct Importer {
        path: &'static str,
        name: &'static str,
        deps: Vec<(&'static str, &'static str, &'static str)>,
    }

    /// Writes each importer's `package.json` and returns the `pnpm-lock.yaml` body describing the
    /// workspace, with a `packages:` entry per distinct resolved `name@version`.
    fn workspace(root: &Utf8Path, importers: &[Importer]) -> eyre::Result<String> {
        let mut lock = String::from("lockfileVersion: '9.0'\n\nimporters:\n");
        let mut packages: BTreeSet<(String, String)> = BTreeSet::new();
        for importer in importers {
            let dir = root.join(importer.path);
            std::fs::create_dir_all(&dir)?;
            let deps = importer
                .deps
                .iter()
                .map(|(name, specifier, _)| format!("\"{name}\": \"{specifier}\""))
                .collect::<Vec<_>>()
                .join(", ");
            std::fs::write(
                dir.join("package.json"),
                format!(
                    "{{ \"name\": \"{}\", \"dependencies\": {{ {deps} }} }}",
                    importer.name
                ),
            )?;
            let _ = write!(lock, "\n  {}:\n    dependencies:\n", importer.path);
            for (name, specifier, version) in &importer.deps {
                lock.push_str(&importer_entry(name, specifier, version));
                packages.insert(((*name).to_string(), (*version).to_string()));
            }
        }
        lock.push_str("\npackages:\n");
        for (name, version) in packages {
            lock.push_str(&package_entry(&name, &version));
        }
        Ok(lock)
    }

    /// A lock mapping key as pnpm writes it: a scoped name (or a scoped `name@version`) starts with
    /// `@`, which YAML cannot take bare, so pnpm single-quotes it.
    fn yaml_key(key: &str) -> String {
        if key.starts_with('@') {
            format!("'{key}'")
        } else {
            key.to_string()
        }
    }

    fn importer_entry(name: &str, specifier: &str, version: &str) -> String {
        // pnpm quotes a protocol specifier (`'catalog:'`), whose colon YAML would otherwise read
        // as a nested mapping.
        let specifier = if specifier.contains(':') {
            format!("'{specifier}'")
        } else {
            specifier.to_string()
        };
        format!(
            "      {}:\n        specifier: {specifier}\n        version: {version}\n",
            yaml_key(name)
        )
    }

    fn package_entry(name: &str, version: &str) -> String {
        format!(
            "\n  {}:\n    resolution: {{integrity: sha512-{}}}\n",
            yaml_key(&format!("{name}@{version}")),
            format!("{name}{version}").replace(['@', '/'], "")
        )
    }

    /// `lock` with `importer`'s `name` entry re-recorded at `to` — specifier and version, the way
    /// pnpm rewrites an importer entry after a resolve — and the `packages:` section brought in
    /// line: the target's entry added when missing, the source's dropped when no importer keeps it.
    fn moved(
        lock: &str,
        importer: &str,
        name: &str,
        (from_spec, from): (&str, &str),
        (to_spec, to): (&str, &str),
    ) -> String {
        // The importer's block: its header line, then every following line indented deeper than the
        // two spaces an importer key sits at (blank lines included).
        let header = format!("  {importer}:");
        let mut lines: Vec<String> = lock.lines().map(str::to_string).collect();
        let start = lines
            .iter()
            .position(|line| *line == header)
            .expect("the importer is in the lock");
        let end = lines[start + 1..]
            .iter()
            .position(|line| !line.is_empty() && !line.starts_with("    "))
            .map_or(lines.len(), |offset| start + 1 + offset);
        let block = lines[start..end].join("\n") + "\n";
        let old_entry = importer_entry(name, from_spec, from);
        assert!(
            block.contains(&old_entry),
            "{importer} declares {name}@{from}: {block}"
        );
        let block = block.replace(&old_entry, &importer_entry(name, to_spec, to));
        lines.splice(start..end, block.lines().map(str::to_string));
        let mut lock = lines.join("\n") + "\n";
        let target = package_entry(name, to);
        if !lock.contains(&target) {
            lock.push_str(&target);
        }
        if !lock.contains(&format!("version: {from}\n")) {
            lock = lock.replace(&package_entry(name, from), "");
        }
        lock
    }

    /// A scripted `pnpm` that logs every invocation to `legs.log`, answers `config get` with
    /// `null`, and otherwise runs `body` (a `case "$*"` arm list) — an unmatched invocation fails.
    fn fake_pnpm(root: &Utf8Path, body: &str) -> eyre::Result<Utf8PathBuf> {
        let script = root.join("fake-pnpm.sh");
        std::fs::write(
            &script,
            formatdoc! {r#"
                #!/bin/sh
                echo "$*" >> legs.log
                case "$*" in
                  *"config get"*)
                    echo 'null'; exit 0 ;;
                {body}
                esac
                echo "unexpected args: $*" >&2; exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(script)
    }

    fn tool_with(script: &Utf8Path) -> eyre::Result<NpmTool<Pnpm>> {
        let cache = tempfile::tempdir()?;
        let mut tool = NpmTool::<Pnpm>::from_http(cooldown_registry::SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        Ok(tool)
    }

    fn project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        }
    }

    fn member(name: &str, path: &str) -> MemberRef {
        MemberRef {
            name: name.to_string(),
            path: path.to_string(),
        }
    }

    fn change(name: &str, from: &str, to: &str, members: &[(&str, &str)]) -> Change {
        Change {
            package: PackageId::new(Pnpm::ID, name, Some(NPM.to_string())),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: members
                .iter()
                .map(|(name, path)| member(name, path))
                .collect(),
        }
    }

    fn tempdir_root() -> eyre::Result<(tempfile::TempDir, Utf8PathBuf)> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        Ok((dir, root))
    }

    async fn apply(tool: &NpmTool<Pnpm>, project: &Project, plan: Plan) -> Result<ApplyReport> {
        let mutation = PreparedMutation::prepare(tool, project, &plan).await?;
        tool.apply(&mutation).await
    }

    fn legs(root: &Utf8Path) -> Vec<String> {
        std::fs::read_to_string(root.join("legs.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn applied_names(report: &ApplyReport) -> Vec<&str> {
        report
            .applied
            .iter()
            .map(|change| change.package.name.as_str())
            .collect()
    }

    fn read(root: &Utf8Path, rel: &str) -> eyre::Result<String> {
        Ok(std::fs::read_to_string(root.join(rel))?)
    }

    /// The solid-js shape: three importers declare one copy, the joint pin lands in two and a
    /// peer-bound third keeps the old version.
    /// The candidate is rolled back — the lock ends with the single pre-apply copy, never both —
    /// and its row names the importers that did and did not take the target, while an unrelated
    /// candidate in the same batch still lands on the re-resolve without it.
    #[tokio::test]
    async fn a_partial_landing_is_rolled_back_and_names_its_importers() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let tool = tool_with(&solid_js_fixture(&root)?)?;
        let members = [
            ("@x/admin", "apps/admin"),
            ("@x/web", "apps/web"),
            ("@x/pdf-view", "packages/pdf-view"),
        ];
        let plan = Plan {
            changes: vec![
                change("solid-js", "1.9.14", "1.9.15", &members),
                change("lodash", "4.17.20", "4.17.21", &members[..1]),
            ],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["lodash"]);
        let [held] = report.skipped.as_slice() else {
            panic!(
                "exactly the partial candidate is held: {:?}",
                report.skipped
            );
        };
        assert_eq!(held.change.package.name, "solid-js");
        assert_eq!(held.reason, SkipReason::ResolverConflict);
        let detail = held
            .detail
            .as_deref()
            .expect("the rollback explains itself");
        assert!(detail.contains("2 of 3 importers"), "{detail}");
        assert!(
            detail.contains("@x/admin, @x/web"),
            "the importers that moved: {detail}"
        );
        assert!(
            detail.contains("@x/pdf-view at 1.9.14"),
            "the importer left behind: {detail}"
        );
        assert!(detail.contains("rolled back"), "{detail}");
        let final_lock = read(&root, "pnpm-lock.yaml")?;
        assert!(
            final_lock.contains("solid-js@1.9.14:") && !final_lock.contains("solid-js@1.9.15"),
            "the workspace keeps its single solid-js copy: {final_lock}"
        );
        assert!(final_lock.contains("lodash@4.17.21:"));
        let legs = legs(&root);
        assert_eq!(
            legs.len(),
            2,
            "one resolve with the pin, one without: {legs:?}"
        );
        assert!(legs[0].contains("solid-js@1.9.15") && legs[0].contains("lodash@4.17.21"));
        assert!(
            !legs[1].contains("solid-js@"),
            "the rejected pin is not retried: {legs:?}"
        );
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        Ok(())
    }

    /// Three importers on `solid-js@^1.9.14` (admin also on `lodash@^4.17.20`), and a scripted pnpm
    /// whose joint pin lands solid-js in admin and web only — pdf-view keeps 1.9.14 — while a
    /// resolve without the solid-js pin moves just lodash.
    /// Returns the script path.
    fn solid_js_fixture(root: &Utf8Path) -> eyre::Result<Utf8PathBuf> {
        let solid = ("^1.9.14", "1.9.14");
        let lock = workspace(
            root,
            &[
                Importer {
                    path: "apps/admin",
                    name: "@x/admin",
                    deps: vec![
                        ("solid-js", solid.0, solid.1),
                        ("lodash", "^4.17.20", "4.17.20"),
                    ],
                },
                Importer {
                    path: "apps/web",
                    name: "@x/web",
                    deps: vec![("solid-js", solid.0, solid.1)],
                },
                Importer {
                    path: "packages/pdf-view",
                    name: "@x/pdf-view",
                    deps: vec![("solid-js", solid.0, solid.1)],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let lodash_only = moved(
            &lock,
            "apps/admin",
            "lodash",
            ("^4.17.20", "4.17.20"),
            ("^4.17.20", "4.17.21"),
        );
        let partial = moved(
            &lodash_only,
            "apps/admin",
            "solid-js",
            solid,
            ("^1.9.14", "1.9.15"),
        );
        let partial = moved(
            &partial,
            "apps/web",
            "solid-js",
            solid,
            ("^1.9.14", "1.9.15"),
        );
        assert!(partial.contains("solid-js@1.9.15:") && partial.contains("solid-js@1.9.14:"));
        std::fs::write(root.join("partial.yaml"), partial)?;
        std::fs::write(root.join("lodash-only.yaml"), lodash_only)?;
        fake_pnpm(
            root,
            indoc! {r#"
                  *"solid-js@1.9.15"*)
                    cp partial.yaml pnpm-lock.yaml; exit 0 ;;
                  *"lodash@4.17.21"*)
                    cp lodash-only.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )
    }

    /// A name the resolver splits on its own — an in-range float of an unpinned dependency in one
    /// importer — is never committed: the batch fails with the split named, so the caller's
    /// candidate isolation reports which candidate's landing caused it instead of certifying a lock
    /// with a duplicate copy no row accounts for.
    #[tokio::test]
    async fn a_split_the_resolver_introduces_fails_the_batch() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "apps/a",
                    name: "a",
                    deps: vec![
                        ("vite", "^6.0.0", "6.4.3"),
                        ("lodash", "^4.17.20", "4.17.20"),
                    ],
                },
                Importer {
                    path: "apps/b",
                    name: "b",
                    deps: vec![("vite", "^6.0.0", "6.4.3")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let floated = moved(
            &lock,
            "apps/a",
            "lodash",
            ("^4.17.20", "4.17.20"),
            ("^4.17.20", "4.17.21"),
        );
        let floated = moved(
            &floated,
            "apps/a",
            "vite",
            ("^6.0.0", "6.4.3"),
            ("^6.0.0", "6.5.0"),
        );
        std::fs::write(root.join("floated.yaml"), floated)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp floated.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("lodash", "4.17.20", "4.17.21", &[("a", "apps/a")])],
            ..Plan::default()
        };

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("a resolver-introduced split must not be committed");

        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("a split is a non-local rejection for candidate isolation, got {error:?}");
        };
        assert!(detail.contains("vite"), "{detail}");
        assert!(
            detail.contains("6.4.3 in apps/b") && detail.contains("6.5.0 in apps/a"),
            "the importers on each side: {detail}"
        );
        Ok(())
    }

    /// The `exclude-folders` shape: `app` and `legacy` both declare `mongoose@^9.8.0`, resolved at
    /// 9.9.1 and 9.8.0.
    /// With `legacy` excluded its copy is neither split evidence nor a pin target: `app` is pinned
    /// alone (the update is filtered to it) and `legacy` stays at 9.8.0.
    /// With nothing excluded, the one shared range admits the target, so both importers are pinned
    /// — the split rule is target-aware, and the second line no longer holds the first.
    #[tokio::test]
    async fn an_excluded_importer_is_neither_split_evidence_nor_a_pin_target() -> eyre::Result<()> {
        for excluded in [true, false] {
            let (_dir, root) = tempdir_root()?;
            let lock = mongoose_workspace(&root, "9.9.1")?;
            // The scripted resolve moves exactly the filtered importers — pnpm's `--filter`
            // contract — and leaves the other one alone.
            let app_only = moved(
                &lock,
                "app",
                "mongoose",
                ("^9.8.0", "9.9.1"),
                ("^9.8.0", "9.9.3"),
            );
            std::fs::write(root.join("app-only.yaml"), &app_only)?;
            let both = moved(
                &app_only,
                "legacy",
                "mongoose",
                ("^9.8.0", "9.8.0"),
                ("^9.8.0", "9.9.3"),
            );
            std::fs::write(root.join("both.yaml"), both)?;
            let script = fake_pnpm(
                &root,
                indoc! {r#"
                      *"--filter ./app --filter ./legacy"*)
                        cp both.yaml pnpm-lock.yaml; exit 0 ;;
                      *"--filter ./app --fail-if-no-match"*)
                        cp app-only.yaml pnpm-lock.yaml; exit 0 ;;
                "#},
            )?;
            let tool = tool_with(&script)?;
            let app = change("mongoose", "9.9.1", "9.9.3", &[("app", "app")]);
            let plan = if excluded {
                Plan {
                    changes: vec![app],
                    excluded_members: vec![member("legacy", "legacy")],
                    ..Plan::default()
                }
            } else {
                Plan {
                    changes: vec![
                        app,
                        change("mongoose", "9.8.0", "9.9.3", &[("legacy", "legacy")]),
                    ],
                    ..Plan::default()
                }
            };

            let report = apply(&tool, &project(&root), plan).await?;

            assert!(
                report.skipped.is_empty(),
                "excluded={excluded}: {:?}",
                report.skipped
            );
            let final_lock = read(&root, "pnpm-lock.yaml")?;
            let legs = legs(&root);
            assert_eq!(legs.len(), 1, "excluded={excluded}: {legs:?}");
            if excluded {
                assert_eq!(applied_names(&report), vec!["mongoose"]);
                assert!(!legs[0].contains("./legacy"), "not a pin target: {legs:?}");
                assert!(
                    final_lock.contains("mongoose@9.8.0:")
                        && final_lock.contains("mongoose@9.9.3:"),
                    "legacy is untouched at 9.8.0: {final_lock}"
                );
                assert!(
                    report.warnings.is_empty(),
                    "the workspace already held two copies, so nothing new is reported: {:?}",
                    report.warnings
                );
            } else {
                assert_eq!(applied_names(&report), vec!["mongoose", "mongoose"]);
                assert!(legs[0].contains("./legacy"), "{legs:?}");
                assert!(
                    !final_lock.contains("mongoose@9.8.0:"),
                    "converged: {final_lock}"
                );
            }
        }
        Ok(())
    }

    /// `app` and `legacy` both on `mongoose@^9.8.0`, `app` resolved at `app_version` and `legacy`
    /// at 9.8.0.
    fn mongoose_workspace(root: &Utf8Path, app_version: &'static str) -> eyre::Result<String> {
        let lock = workspace(
            root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("mongoose", "^9.8.0", app_version)],
                },
                Importer {
                    path: "legacy",
                    name: "legacy",
                    deps: vec![("mongoose", "^9.8.0", "9.8.0")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        Ok(lock)
    }

    /// An excluded importer left at the version every importer shared before the run makes the name
    /// resolve at two versions afterwards.
    /// That is what the exclusion asked for, so the lock is kept — but the new second copy is
    /// reported, never silent.
    #[tokio::test]
    async fn an_excluded_importer_left_behind_is_reported_not_rolled_back() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = mongoose_workspace(&root, "9.8.0")?;
        let app_only = moved(
            &lock,
            "app",
            "mongoose",
            ("^9.8.0", "9.8.0"),
            ("^9.8.0", "9.9.3"),
        );
        std::fs::write(root.join("app-only.yaml"), app_only)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp app-only.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("mongoose", "9.8.0", "9.9.3", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["mongoose"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let [warning] = report.warnings.as_slice() else {
            panic!(
                "the new second copy is reported once: {:?}",
                report.warnings
            );
        };
        assert_eq!(warning.kind, DiagnosticKind::Held);
        assert_eq!(warning.package.as_deref(), Some("mongoose"));
        assert!(warning.message.contains("legacy"), "{}", warning.message);
        assert!(warning.message.contains("excluded"), "{}", warning.message);
        assert!(
            warning.message.contains("9.8.0") && warning.message.contains("9.9.3"),
            "{}",
            warning.message
        );
        Ok(())
    }

    /// pnpm re-resolves the named package in every importer whose range admits a newer version
    /// whatever the update's filter, so the excluded importer's `^9.8.0` copy moves too: the
    /// upgrade lands (an excluded subtree must not veto it) and the move is reported with the
    /// package attached, with the excluded manifest untouched.
    #[tokio::test]
    async fn an_excluded_importer_re_resolved_in_range_is_reported_not_rolled_back()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = mongoose_workspace(&root, "9.9.1")?;
        let both = moved(
            &lock,
            "app",
            "mongoose",
            ("^9.8.0", "9.9.1"),
            ("^9.8.0", "9.9.3"),
        );
        let both = moved(
            &both,
            "legacy",
            "mongoose",
            ("^9.8.0", "9.8.0"),
            ("^9.8.0", "9.9.3"),
        );
        std::fs::write(root.join("both.yaml"), both)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp both.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("mongoose", "9.9.1", "9.9.3", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            ..Plan::default()
        };

        let legacy_manifest = read(&root, "legacy/package.json")?;
        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["mongoose"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let [warning] = report.warnings.as_slice() else {
            panic!(
                "the re-resolved excluded copy is reported once: {:?}",
                report.warnings
            );
        };
        assert_eq!(warning.package.as_deref(), Some("mongoose"));
        assert!(
            warning
                .message
                .contains("mongoose in legacy (9.8.0 → 9.9.3)"),
            "{}",
            warning.message
        );
        assert!(
            warning.message.contains("re-resolved"),
            "{}",
            warning.message
        );
        assert_eq!(read(&root, "legacy/package.json")?, legacy_manifest);
        assert!(read(&root, "pnpm-lock.yaml")?.contains("version: 9.9.3"));
        Ok(())
    }

    /// Two importers on `^4.17.20` and `^4.17.21`, both at 4.17.21, with a matured 4.17.22: every
    /// declared range admits the target, so the name is pinned in both importers and neither
    /// manifest changes — what a plain `pnpm update` would do.
    #[tokio::test]
    async fn ranges_that_all_admit_the_target_are_pinned_without_touching_manifests()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "apps/a",
                    name: "a",
                    deps: vec![("lodash", "^4.17.20", "4.17.21")],
                },
                Importer {
                    path: "apps/b",
                    name: "b",
                    deps: vec![("lodash", "^4.17.21", "4.17.21")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let landed = moved(
            &lock,
            "apps/a",
            "lodash",
            ("^4.17.20", "4.17.21"),
            ("^4.17.20", "4.17.22"),
        );
        let landed = moved(
            &landed,
            "apps/b",
            "lodash",
            ("^4.17.21", "4.17.21"),
            ("^4.17.21", "4.17.22"),
        );
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"lodash@4.17.22"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let manifests_before = (
            read(&root, "apps/a/package.json")?,
            read(&root, "apps/b/package.json")?,
        );
        let plan = Plan {
            changes: vec![change(
                "lodash",
                "4.17.21",
                "4.17.22",
                &[("a", "apps/a"), ("b", "apps/b")],
            )],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["lodash"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let legs = legs(&root);
        assert_eq!(legs.len(), 1, "{legs:?}");
        assert!(legs[0].contains("--filter ./apps/a") && legs[0].contains("--filter ./apps/b"));
        assert_eq!(
            (
                read(&root, "apps/a/package.json")?,
                read(&root, "apps/b/package.json")?
            ),
            manifests_before,
            "both ranges already admit the target, so neither is rewritten"
        );
        Ok(())
    }

    /// `~7.3.0` beside `^7.0.0`: a target the tilde range excludes is held on its own line, with
    /// the row naming the ranges, the target, and `--rewrite` as the way to converge — never
    /// calling the ranges "incompatible" — and pnpm is not asked to move it.
    /// A target both admit lands.
    #[tokio::test]
    async fn a_range_that_excludes_the_target_holds_the_name_with_an_actionable_row()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "pkgs/tilde",
                    name: "tilde",
                    deps: vec![("semver", "~7.3.0", "7.3.8")],
                },
                Importer {
                    path: "pkgs/caret",
                    name: "caret",
                    deps: vec![("semver", "^7.0.0", "7.3.8")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let patched = moved(
            &lock,
            "pkgs/tilde",
            "semver",
            ("~7.3.0", "7.3.8"),
            ("~7.3.0", "7.3.9"),
        );
        let patched = moved(
            &patched,
            "pkgs/caret",
            "semver",
            ("^7.0.0", "7.3.8"),
            ("^7.0.0", "7.3.9"),
        );
        std::fs::write(root.join("patched.yaml"), patched)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"semver@7.3.9"*)
                    cp patched.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let members = [("tilde", "pkgs/tilde"), ("caret", "pkgs/caret")];
        let plan = |to: &str| Plan {
            changes: vec![change("semver", "7.3.8", to, &members)],
            ..Plan::default()
        };

        let held = apply(&tool, &project(&root), plan("7.4.0")).await?;
        assert!(held.applied.is_empty());
        let [skip] = held.skipped.as_slice() else {
            panic!("{:?}", held.skipped);
        };
        assert_eq!(skip.reason, SkipReason::MultiVersionHeld);
        let detail = skip.detail.as_deref().expect("detail");
        assert!(
            detail.contains("~7.3.0") && detail.contains("^7.0.0"),
            "{detail}"
        );
        assert!(
            detail.contains("7.4.0"),
            "the target the range excludes: {detail}"
        );
        assert!(detail.contains("--rewrite"), "the way forward: {detail}");
        assert!(!detail.contains("incompatible"), "{detail}");
        assert!(
            legs(&root).is_empty(),
            "a held split is never handed to pnpm"
        );

        let landed = apply(&tool, &project(&root), plan("7.3.9")).await?;
        assert_eq!(applied_names(&landed), vec!["semver"]);
        assert!(landed.skipped.is_empty(), "{:?}", landed.skipped);
        Ok(())
    }

    /// `--rewrite` converges a genuine split: `tailwind-merge@^2.6.0` in one importer beside
    /// `^3.6.0` in another is pinned to 3.6.0 and the narrower range widened, while the importer
    /// already on 3.6.0 is left alone.
    /// Without `--rewrite` the same plan is held.
    #[tokio::test]
    async fn rewrite_converges_a_split_by_widening_the_declaring_range() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "packages/utils",
                    name: "@x/utils",
                    deps: vec![("tailwind-merge", "^2.6.0", "2.6.1")],
                },
                Importer {
                    path: "apps/web",
                    name: "@x/web",
                    deps: vec![("tailwind-merge", "^3.6.0", "3.6.0")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        // pnpm copies the (widened) manifest range into the importer entry and drops the old copy.
        let converged = moved(
            &lock,
            "packages/utils",
            "tailwind-merge",
            ("^2.6.0", "2.6.1"),
            ("^3.6.0", "3.6.0"),
        );
        std::fs::write(root.join("converged.yaml"), converged)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"--filter ./packages/utils --fail-if-no-match update tailwind-merge@3.6.0"*)
                    cp converged.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let web_manifest = read(&root, "apps/web/package.json")?;
        let plan = |rewrite| Plan {
            changes: vec![change(
                "tailwind-merge",
                "2.6.1",
                "3.6.0",
                &[("@x/utils", "packages/utils")],
            )],
            rewrite,
            ..Plan::default()
        };

        let held = apply(&tool, &project(&root), plan(RewriteMode::Auto)).await?;
        assert!(held.applied.is_empty());
        let [skip] = held.skipped.as_slice() else {
            panic!("{:?}", held.skipped);
        };
        assert_eq!(skip.reason, SkipReason::MultiVersionHeld);
        assert!(
            skip.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("--rewrite")),
            "{:?}",
            skip.detail
        );
        assert!(
            legs(&root).is_empty(),
            "without --rewrite the split stays held"
        );
        assert!(read(&root, "packages/utils/package.json")?.contains("^2.6.0"));

        let converged = apply(&tool, &project(&root), plan(RewriteMode::Always)).await?;
        assert_eq!(applied_names(&converged), vec!["tailwind-merge"]);
        assert!(converged.skipped.is_empty(), "{:?}", converged.skipped);
        assert!(
            read(&root, "packages/utils/package.json")?.contains("^3.6.0"),
            "the narrower declaring range is widened to admit the target"
        );
        assert_eq!(
            read(&root, "apps/web/package.json")?,
            web_manifest,
            "the importer already on the target is not rewritten"
        );
        assert!(
            !read(&root, "pnpm-lock.yaml")?.contains("tailwind-merge@2.6.1"),
            "the workspace converges on one copy"
        );
        Ok(())
    }

    /// A split `--rewrite` asks pnpm to converge but pnpm cannot jointly resolve (a peer contract
    /// binds one importer's copy) fails loudly: the resolver's own verdict comes back as the error,
    /// the widened manifests are restored, and the lock is left exactly as it was — never a
    /// half-converged lock that does not install.
    #[tokio::test]
    async fn rewrite_that_cannot_jointly_resolve_fails_loudly_and_restores_manifests()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "packages/utils",
                    name: "@x/utils",
                    deps: vec![("tailwind-merge", "^2.6.0", "2.6.1")],
                },
                Importer {
                    path: "apps/web",
                    name: "@x/web",
                    deps: vec![("tailwind-merge", "^3.6.0", "3.6.0")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"update tailwind-merge@3.6.0"*)
                    echo "ERR_PNPM_PEER_DEP_ISSUES  Unmet peer dependencies" >&2; exit 1 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let utils_manifest = read(&root, "packages/utils/package.json")?;
        let web_manifest = read(&root, "apps/web/package.json")?;
        let plan = Plan {
            changes: vec![change(
                "tailwind-merge",
                "2.6.1",
                "3.6.0",
                &[("@x/utils", "packages/utils")],
            )],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("a split pnpm cannot converge must not be committed");

        // The resolver's own verdict is what surfaces, so candidate isolation reports it as the
        // held row's detail.
        let CoreError::Tool { stderr, .. } = error else {
            panic!("{error:?}");
        };
        assert!(stderr.contains("ERR_PNPM_PEER_DEP_ISSUES"), "{stderr}");
        assert_eq!(legs(&root).len(), 1, "{:?}", legs(&root));
        // The widen that preceded the resolve is undone with it, and the lock is untouched.
        assert_eq!(read(&root, "packages/utils/package.json")?, utils_manifest);
        assert_eq!(read(&root, "apps/web/package.json")?, web_manifest);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        Ok(())
    }

    /// A name planned at two targets — `^22` and `^25` lines each advancing within themselves —
    /// cannot be pinned by one joint update even under `--rewrite`; both rows stay held and say
    /// what would converge them.
    #[tokio::test]
    async fn rewrite_cannot_converge_a_name_planned_at_two_targets() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "pkgs/low",
                    name: "low",
                    deps: vec![("@types/node", "^22.0.0", "22.19.20")],
                },
                Importer {
                    path: "pkgs/high",
                    name: "high",
                    deps: vec![("@types/node", "^25.0.0", "25.9.2")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let script = fake_pnpm(&root, "")?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![
                change(
                    "@types/node",
                    "22.19.20",
                    "22.19.21",
                    &[("low", "pkgs/low")],
                ),
                change("@types/node", "25.9.2", "25.9.3", &[("high", "pkgs/high")]),
            ],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert!(report.applied.is_empty());
        assert_eq!(report.skipped.len(), 2, "{:?}", report.skipped);
        for skip in &report.skipped {
            assert_eq!(skip.reason, SkipReason::MultiVersionHeld);
            let detail = skip.detail.as_deref().expect("detail");
            assert!(detail.contains("several targets"), "{detail}");
            assert!(detail.contains("22.19.21, 25.9.3"), "{detail}");
            assert!(detail.contains("--major"), "{detail}");
            // The ranges are not to blame and `--rewrite` was already passed, so the row must say
            // neither.
            assert!(!detail.contains("pass --rewrite"), "{detail}");
            assert!(!detail.contains("not every declared range"), "{detail}");
        }
        assert!(legs(&root).is_empty());
        Ok(())
    }

    /// A single permissive range admitting both of a name's planned targets holds the name all the
    /// same, under either rewrite mode: one joint update cannot pin one name to two versions, and
    /// judged by the ranges alone the two pins would reach pnpm together.
    /// One target for every line (`--major`) releases it, since every range admits that target.
    #[test]
    fn a_name_planned_at_two_targets_is_held_whatever_its_ranges_admit() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "pkgs/low",
                    name: "low",
                    deps: vec![("@types/node", ">=22", "22.19.20")],
                },
                Importer {
                    path: "pkgs/high",
                    name: "high",
                    deps: vec![("@types/node", ">=22", "25.9.2")],
                },
            ],
        )?;
        let low = [("low", "pkgs/low")];
        let high = [("high", "pkgs/high")];
        for rewrite in [RewriteMode::Auto, RewriteMode::Always] {
            let two_targets = Plan {
                changes: vec![
                    change("@types/node", "22.19.20", "22.19.21", &low),
                    change("@types/node", "25.9.2", "25.9.3", &high),
                ],
                rewrite,
                ..Plan::default()
            };
            assert!(
                held_split_names::<Pnpm>(Some(&lock), &two_targets).contains("@types/node"),
                "{rewrite:?}"
            );
        }
        let one_target = Plan {
            changes: vec![
                change("@types/node", "22.19.20", "25.9.3", &low),
                change("@types/node", "25.9.2", "25.9.3", &high),
            ],
            ..Plan::default()
        };
        assert!(held_split_names::<Pnpm>(Some(&lock), &one_target).is_empty());
        // The multi-target hold needs no lock: without one the two pins would otherwise reach
        // pnpm together.
        let two_targets = Plan {
            changes: vec![
                change("@types/node", "22.19.20", "22.19.21", &low),
                change("@types/node", "25.9.2", "25.9.3", &high),
            ],
            ..Plan::default()
        };
        assert!(held_split_names::<Pnpm>(None, &two_targets).contains("@types/node"));
        assert!(held_split_names::<Pnpm>(None, &one_target).is_empty());
        Ok(())
    }

    /// A second copy the resolve adds below the importers' view — a transitive requirement pulling
    /// an older line while every importer keeps the newest — changes neither the importer entries
    /// nor the newest-copy diff, so it is named by its own warning rather than passing silently.
    #[tokio::test]
    async fn a_transitive_second_copy_the_resolve_adds_is_reported() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[Importer {
                path: "app",
                name: "app",
                deps: vec![("bar", "^1.0.0", "1.0.0"), ("stateful", "^1.9.0", "1.9.14")],
            }],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        // `bar@1.1.0` requires an older `stateful` line, so the graph gains `stateful@1.8.0` while
        // the importer stays on 1.9.14.
        let mut settled = moved(
            &lock,
            "app",
            "bar",
            ("^1.0.0", "1.0.0"),
            ("^1.0.0", "1.1.0"),
        );
        settled.push_str(&package_entry("stateful", "1.8.0"));
        std::fs::write(root.join("settled.yaml"), settled)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp settled.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("bar", "1.0.0", "1.1.0", &[("app", "app")])],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["bar"]);
        let [warning] = report.warnings.as_slice() else {
            panic!("the new copy is reported once: {:?}", report.warnings);
        };
        assert_eq!(warning.package.as_deref(), Some("stateful"));
        assert!(
            warning.message.contains("(1.9.14)") && warning.message.contains("1.8.0, 1.9.14"),
            "{}",
            warning.message
        );
        Ok(())
    }

    /// A line declared through a `catalog:` specifier is held with the catalog row even when a
    /// sibling line declares the name with a plain range: the sibling still lands within its range,
    /// and the catalog line is never pinned or reported as a resolver conflict.
    #[tokio::test]
    async fn a_catalog_line_is_held_while_the_plain_range_line_lands() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "apps/a",
                    name: "a",
                    deps: vec![("react", "catalog:", "18.2.0")],
                },
                Importer {
                    path: "apps/b",
                    name: "b",
                    deps: vec![("react", "^18.0.0", "18.3.1")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let landed = moved(
            &lock,
            "apps/b",
            "react",
            ("^18.0.0", "18.3.1"),
            ("^18.0.0", "18.3.2"),
        );
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"--filter ./apps/b --fail-if-no-match update react@18.3.2"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![
                change("react", "18.2.0", "18.3.2", &[("a", "apps/a")]),
                change("react", "18.3.1", "18.3.2", &[("b", "apps/b")]),
            ],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        let [applied] = report.applied.as_slice() else {
            panic!("{:?}", report.applied);
        };
        assert_eq!(applied.from.as_str(), "18.3.1");
        let [held] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(held.change.from.as_str(), "18.2.0");
        assert_eq!(held.reason, SkipReason::NotEligible);
        assert!(
            held.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("catalog-managed")),
            "{:?}",
            held.detail
        );
        assert_eq!(legs(&root).len(), 1, "the catalog line never reaches pnpm");
        Ok(())
    }

    /// An `npm:` alias in an excluded importer is compared under its own declared name, so its
    /// re-resolution is seen like any other entry: its specifier is no range cooldown can judge,
    /// which proves nothing, so the move is reported as re-resolved rather than refused as drift.
    #[tokio::test]
    async fn an_alias_in_an_excluded_importer_is_reported_when_it_moves() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let alias = ("npm:lodash@^4.17.0", "lodash@4.17.21");
        // pnpm keys the alias's package by the real name; the fixture helper keys it by the
        // declared name, so the package keys are corrected on both locks.
        let real_keys = |lock: String| {
            lock.replace("my-lodash@lodash@4.17.21:", "lodash@4.17.21:")
                .replace("my-lodash@lodash@4.17.22:", "lodash@4.17.22:")
        };
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("bar", "^1.0.0", "1.0.0")],
                },
                Importer {
                    path: "legacy",
                    name: "legacy",
                    deps: vec![("my-lodash", alias.0, alias.1)],
                },
            ],
        )?;
        let settled = moved(
            &lock,
            "app",
            "bar",
            ("^1.0.0", "1.0.0"),
            ("^1.0.0", "1.1.0"),
        );
        let settled = moved(
            &settled,
            "legacy",
            "my-lodash",
            alias,
            ("npm:lodash@^4.17.0", "lodash@4.17.22"),
        );
        std::fs::write(root.join("pnpm-lock.yaml"), real_keys(lock))?;
        std::fs::write(root.join("settled.yaml"), real_keys(settled))?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp settled.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("bar", "1.0.0", "1.1.0", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        // The real package's move also surfaces as a collateral row under its own name, so the
        // report shows both the package that moved and the excluded declaration that carried it.
        assert_eq!(applied_names(&report), vec!["bar", "lodash"]);
        assert!(
            report
                .applied
                .iter()
                .any(|change| change.package.name == "lodash" && !change.direct),
            "{:?}",
            report.applied
        );
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let [warning] = report.warnings.as_slice() else {
            panic!("the alias move is reported once: {:?}", report.warnings);
        };
        assert_eq!(warning.package.as_deref(), Some("my-lodash"));
        assert!(
            warning
                .message
                .contains("my-lodash in legacy (lodash@4.17.21 → lodash@4.17.22)"),
            "{}",
            warning.message
        );
        Ok(())
    }

    /// The manifest facts of a row follow its managed members once the excluded ones are dropped:
    /// an excluded importer's plain range no longer cancels a managed exact pin, and its explicit
    /// upper bound no longer holds a managed range that admits the target.
    #[tokio::test]
    async fn excluded_importers_neither_pin_nor_bound_managed_rows() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        workspace(
            &root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("mongoose", "9.8.0", "9.8.0"), ("vite", "^6.0.0", "6.4.3")],
                },
                Importer {
                    path: "legacy",
                    name: "legacy",
                    deps: vec![("mongoose", "^9.8.0", "9.8.0"), ("vite", "<6.5.0", "6.4.3")],
                },
            ],
        )
        .and_then(|lock| Ok(std::fs::write(root.join("pnpm-lock.yaml"), lock)?))?;
        let script = fake_pnpm(&root, "")?;
        let tool = tool_with(&script)?;
        let project = project(&root);
        let mut deps = tool.dependencies(&project, DepScope::Graph).await?;
        let row = |deps: &[Dependency], name: &str| {
            deps.iter()
                .find(|dep| dep.package.name == name)
                .map(|dep| (dep.pinned, dep.declared_bound.clone()))
                .expect("declared row")
        };
        // Read raw, the excluded importer's declarations still count.
        assert_eq!(row(&deps, "mongoose"), (false, None));
        assert_eq!(row(&deps, "vite"), (false, Some("<6.5.0".to_string())));

        for dep in &mut deps {
            dep.members.retain(|member| member.path != "legacy");
        }
        tool.rescope_members(&project, &mut deps, &[member("legacy", "legacy")])
            .await?;

        assert_eq!(row(&deps, "mongoose"), (true, None));
        assert_eq!(row(&deps, "vite"), (false, None));
        Ok(())
    }

    /// A transitive copy of a name only an excluded importer declares is not "declared elsewhere":
    /// the excluded declaration is not the run's, so the copy keeps the override leg instead of
    /// being held on its own line.
    #[test]
    fn excluded_declarations_do_not_hold_a_transitive_copy() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("bar", "^1.0.0", "1.0.0")],
                },
                Importer {
                    path: "legacy",
                    name: "legacy",
                    deps: vec![("foo", "^2.0.0", "2.1.0")],
                },
            ],
        )?;
        let everyone = managed_importer_declarations::<Pnpm>(Some(&lock), &HashSet::new());
        assert!(everyone.contains("foo") && everyone.contains("bar"));
        let excluded = HashSet::from(["legacy".to_string()]);
        let managed = managed_importer_declarations::<Pnpm>(Some(&lock), &excluded);
        assert!(managed.contains("bar"));
        assert!(!managed.contains("foo"));
        assert!(managed_importer_declarations::<Pnpm>(None, &excluded).is_empty());
        Ok(())
    }

    /// A settlement that re-declares an excluded importer's dependency — its `specifier:` changes,
    /// which pnpm never does on its own — is refused: that is a stale excluded importer refreshed or
    /// an override that reached it, not a unification within the declared range.
    /// The importer that merely stayed in place is not named.
    #[tokio::test]
    async fn a_settlement_that_re_declares_an_excluded_importer_is_refused() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let foo = ("^1.0.0", "1.0.0");
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("bar", "^1.0.0", "1.0.0"), ("foo", foo.0, foo.1)],
                },
                Importer {
                    path: "legacy-a",
                    name: "legacy-a",
                    deps: vec![("foo", foo.0, foo.1)],
                },
                Importer {
                    path: "legacy-b",
                    name: "legacy-b",
                    deps: vec![("foo", foo.0, foo.1)],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        // The planned `bar` pin lands, and the settlement re-declares `foo` in the excluded
        // `legacy-a` (a new specifier, as a refreshed stale importer would show) while `legacy-b`
        // stays.
        let settled = moved(
            &lock,
            "app",
            "bar",
            ("^1.0.0", "1.0.0"),
            ("^1.0.0", "1.1.0"),
        );
        let settled = moved(&settled, "legacy-a", "foo", foo, ("^2.0.0", "2.0.0"));
        std::fs::write(root.join("settled.yaml"), settled)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp settled.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("bar", "1.0.0", "1.1.0", &[("app", "app")])],
            excluded_members: vec![
                member("legacy-a", "legacy-a"),
                member("legacy-b", "legacy-b"),
            ],
            ..Plan::default()
        };

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("a re-declared excluded importer must not be committed");

        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("{error:?}");
        };
        assert!(
            detail.contains("foo in legacy-a (declared ^1.0.0 → ^2.0.0, 1.0.0 → 2.0.0)"),
            "the drift is named by importer: {detail}"
        );
        assert!(!detail.contains("legacy-b"), "{detail}");
        Ok(())
    }

    /// Two lines of one name converging on one target under `--rewrite`: rolling back the line
    /// whose pin landed in only some of its importers must not hide the applied row of the sibling
    /// line that landed everywhere, and pnpm gets the shared pin once.
    #[tokio::test]
    async fn a_rolled_back_line_does_not_hide_its_sibling_on_the_same_target() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "apps/a",
                    name: "a",
                    deps: vec![("foo", "^1.0.0", "1.0.0")],
                },
                Importer {
                    path: "apps/b",
                    name: "b",
                    deps: vec![("foo", "^2.0.0", "2.0.0")],
                },
                Importer {
                    path: "apps/c",
                    name: "c",
                    deps: vec![("foo", "^2.0.0", "2.0.0")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        // The joint pin lands `a` and `b` but leaves `c`; the retry without the `b`/`c` line lands
        // `a` alone.
        let a_only = moved(
            &lock,
            "apps/a",
            "foo",
            ("^1.0.0", "1.0.0"),
            ("^3.0.0", "3.0.0"),
        );
        let a_and_b = moved(
            &a_only,
            "apps/b",
            "foo",
            ("^2.0.0", "2.0.0"),
            ("^3.0.0", "3.0.0"),
        );
        std::fs::write(root.join("a-and-b.yaml"), a_and_b)?;
        std::fs::write(root.join("a-only.yaml"), a_only)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"--filter ./apps/a --filter ./apps/b --filter ./apps/c --fail-if-no-match update foo@3.0.0 --lockfile-only"*)
                    cp a-and-b.yaml pnpm-lock.yaml; exit 0 ;;
                  *"--filter ./apps/a --fail-if-no-match update foo@3.0.0 --lockfile-only"*)
                    cp a-only.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![
                change("foo", "1.0.0", "3.0.0", &[("a", "apps/a")]),
                change("foo", "2.0.0", "3.0.0", &[("b", "apps/b"), ("c", "apps/c")]),
            ],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        // The `a` line landed and keeps its row; the `b`/`c` line was rolled back with its own row.
        let [applied] = report.applied.as_slice() else {
            panic!("{:?}", report.applied);
        };
        assert_eq!(applied.from.as_str(), "1.0.0");
        let [skip] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(skip.change.from.as_str(), "2.0.0");
        assert_eq!(skip.reason, SkipReason::ResolverConflict);
        assert!(
            skip.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("left c at 2.0.0")),
            "{:?}",
            skip.detail
        );
        assert_eq!(legs(&root).len(), 2, "{:?}", legs(&root));
        // The rolled-back line's manifests are restored; the landed line's widen stays.
        assert!(read(&root, "apps/a/package.json")?.contains("^3.0.0"));
        assert!(read(&root, "apps/b/package.json")?.contains("^2.0.0"));
        assert!(read(&root, "apps/c/package.json")?.contains("^2.0.0"));
        assert!(read(&root, "pnpm-lock.yaml")?.contains("foo@2.0.0:"));
        Ok(())
    }

    /// The exclusion exception is exact: an excluded importer left on the old version is what the
    /// run asked for, but the importers the run manages must still agree on one version — two new
    /// versions among them is a split the resolver introduced, and it fails the batch.
    #[tokio::test]
    async fn in_scope_importers_split_across_new_versions_fail_despite_an_excluded_straggler()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let foo = ("^1.0.0", "1.0.0");
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("foo", foo.0, foo.1)],
                },
                Importer {
                    path: "web",
                    name: "web",
                    deps: vec![("foo", foo.0, foo.1)],
                },
                Importer {
                    path: "legacy",
                    name: "legacy",
                    deps: vec![("foo", foo.0, foo.1)],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        // `app` lands on the target, `web` floats past it, and the excluded `legacy` stays.
        let split = moved(&lock, "app", "foo", foo, ("^1.0.0", "1.2.0"));
        let split = moved(&split, "web", "foo", foo, ("^1.0.0", "1.5.0"));
        std::fs::write(root.join("split.yaml"), split)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp split.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change(
                "foo",
                "1.0.0",
                "1.2.0",
                &[("app", "app"), ("web", "web")],
            )],
            excluded_members: vec![member("legacy", "legacy")],
            ..Plan::default()
        };

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("two in-scope versions are a split whatever the excluded importer did");

        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("{error:?}");
        };
        assert!(
            detail.contains("1.2.0 in app") && detail.contains("1.5.0 in web"),
            "{detail}"
        );
        Ok(())
    }

    /// pnpm advisory identity: the lock grants only a registry-shaped entry (a tarball, git, or
    /// injected-directory resolution is withheld) and only while no readable configuration layer
    /// reroutes it; the feed-time query then has to state the public registry — a stated one keeps
    /// the identities minus any rerouted scope, an unstated registry or a failing query withholds
    /// them all.
    #[tokio::test]
    async fn pnpm_identity_needs_a_registry_entry_and_a_stated_public_registry() -> eyre::Result<()>
    {
        let (_dir, root) = tempdir_root()?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root", "dependencies": { "lodash": "^4.17.0" } }"#,
        )?;
        std::fs::write(
            root.join("pnpm-lock.yaml"),
            indoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      lodash:
                        specifier: ^4.17.0
                        version: 4.17.21

                packages:

                  lodash@4.17.21:
                    resolution: {integrity: sha512-a}

                  private-api@1.0.0:
                    resolution: {integrity: sha512-b, tarball: https://npm.corp.example/private-api/-/private-api-1.0.0.tgz}

                  pinned-git@1.2.0:
                    resolution: {commit: abc123, repo: https://github.com/user/pinned-git.git, type: git}

                  local-shim@file:packages/shim:
                    resolution: {directory: packages/shim, type: directory}
            "},
        )?;
        let project = project(&root);
        let graph = tool_with(&root.join("never-run"))?
            .dependencies(&project, DepScope::Graph)
            .await?;
        let identity_of = |name: &str| {
            graph
                .iter()
                .find(|dep| dep.package.name == name)
                .map(|dep| dep.advisory_identity.clone())
        };
        assert_eq!(
            identity_of("private-api"),
            Some(None),
            "a tarball URL is withheld"
        );
        assert_eq!(
            identity_of("pinned-git"),
            Some(None),
            "a git resolution is withheld"
        );
        assert_eq!(
            identity_of("local-shim"),
            None,
            "an injected workspace copy is not a registry dependency at all"
        );
        // The grant direction depends on the host's own npm configuration layers, which can only
        // veto; the hermetic grant chain is pinned in `npmrc`.
        let host_reroutes = crate::npmrc::RegistryOverrides::read(&root, Pnpm::NATIVE_MIN_AGE_FILE)
            .reroutes("lodash");
        assert_eq!(
            identity_of("lodash"),
            Some((!host_reroutes).then(|| "lodash".to_string())),
            "a registry-shaped entry is granted unless a configuration layer reroutes it"
        );

        let public = indoc! {r#"
              *"config list --json"*)
                echo '{"registry":"https://registry.npmjs.org/","user-agent":"pnpm/10"}'; exit 0 ;;
        "#};
        assert_eq!(
            confirmed_identities(&root, &project, public).await?,
            vec![Some("lodash".to_string()), Some("@corp/api".to_string())],
            "a stated public registry completes the proof for every scope"
        );
        let scoped = indoc! {r#"
              *"config list --json"*)
                echo '{"registry":"https://registry.npmjs.org/","@corp:registry":"https://npm.corp.example/"}'; exit 0 ;;
        "#};
        assert_eq!(
            confirmed_identities(&root, &project, scoped).await?,
            vec![Some("lodash".to_string()), None],
            "a scope override withholds only its scope"
        );
        let unstated = indoc! {r#"
              *"config list --json"*)
                echo '{"user-agent":"pnpm/10"}'; exit 0 ;;
        "#};
        assert_eq!(
            confirmed_identities(&root, &project, unstated).await?,
            vec![None, None],
            "no stated registry, no proof — never the default by assumption"
        );
        let failing = indoc! {r#"
              *"config list --json"*)
                exit 1 ;;
        "#};
        assert_eq!(
            confirmed_identities(&root, &project, failing).await?,
            vec![None, None],
            "a failing query withholds everything"
        );
        Ok(())
    }

    /// A dependency as `dependencies()` would grant it — identity present, everything else minimal
    /// — the confirmation hook's input.
    fn granted(name: &str) -> Dependency {
        Dependency {
            package: PackageId::new(Pnpm::ID, name.to_string(), Some(NPM.to_string())),
            advisory_identity: Some(name.to_string()),
            current: Version::new("1.0.0".to_string()),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    /// The identities of a granted `lodash` and `@corp/api` after confirmation against a scripted
    /// `pnpm config list --json` whose `case` arm is `body`.
    async fn confirmed_identities(
        root: &Utf8Path,
        project: &Project,
        body: &str,
    ) -> eyre::Result<Vec<Option<String>>> {
        let script = fake_pnpm(root, body)?;
        let tool = tool_with(&script)?;
        let mut deps = vec![granted("lodash"), granted("@corp/api")];
        tool.confirm_advisory_identities(project, &mut deps).await;
        Ok(deps.into_iter().map(|dep| dep.advisory_identity).collect())
    }
}
