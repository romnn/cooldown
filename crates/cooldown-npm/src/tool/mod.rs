//! The generic JavaScript/TypeScript [`Tool`](cooldown_core::Tool):
//! detection, the resolved graph from a lockfile, npm registry
//! publish times, and driver-backed re-resolution/apply.
//! The lockfile format and driver binary are supplied by a [`NodeLock`] type
//! parameter, so npm, pnpm, yarn, and bun are all the same adapter specialised
//! over their lock format — they share the npm registry and version model and
//! differ only in how their lock is parsed and how their CLI re-pins a dependency.

use crate::apply::landing::{
    CandidateLanding, CandidateRunner, LandingPlan, OwnedStep, absolute_cutoff_from_project,
    candidate_landing, holds_exact_pins, restore_after_owned_step, target_in_declared_range,
    window_minutes_from_cutoff,
};
use crate::lock::{
    AliasOnlyVersions, EffectiveRegistryQuery, FixedBinding, FixedOwner, MemberAttribution,
    MemberIndex, MemberResolution, NameVersion, NodeLock, SplitHold, SplitVerdict,
    UnattributedName, Unproven, UnprovenDeclaration, WorkspaceDialect, WorkspaceManifests,
    YarnBindings,
};
use crate::manifest;
use crate::native::{
    ConfigStringList, EmptyList, NativeAge, NativeSelectors, exact_exclusion,
    minimum_age_exclusions, prune_exclusions, set_yaml_block_list, set_yaml_scalar,
    set_yaml_string_map, window_minutes,
};
use crate::nodecmd::NodeCmd;
use crate::peers::{
    JointResolve, PeerBaseline, PeerEvidence, PeerPartition, PeerViolations, WorkspacePeer,
    journaled_lock, partition_peer_held, peer_conflict_blocker, peer_held_skip,
    plan_peer_rejections, proven_peer_violations, settle_landed_candidate,
};
use crate::pins::{AuthorizedPins, EditStates, ManifestEdits, ProtectedPins};
use crate::registry::{NPM, NpmRegistry};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{
    RegistryVersionClassifier, build_registry_releases, skipped_on_apply_error,
    verify_current_unknown,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, CoreError, Declaration, DepScope,
    Dependency, Diagnostic, DiagnosticKind, FetchContext, LockStatus, LockVerifyReport, MemberRef,
    MutationBaseline, NativePolicyLayer, PackageId, PackageRegistry, Plan, PreparedMutation,
    Project, ProjectMarker, ProjectMutationJournal, ProjectMutationState, RawRelease, Release,
    ReleaseFetcher, ReleaseOrder, ReleaseQuality, ResolvedPolicy, Result, RewriteMode,
    SingleCopyPolicy, SkipReason, Skipped, SyncReport, SyncScope, TargetFallback, ToolId, ToolRead,
    ToolWrite, UpdateKind, VerifyReport, Version, redact,
};
use cooldown_registry::SharedHttp;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::marker::PhantomData;

struct WholeGraphInputs {
    exact_pins: Vec<(String, String)>,
    importer_filters: Vec<String>,
    /// The exact pins the widen rewrote with the user's authority.
    authorized: AuthorizedPins,
}

/// The accepted lock of a verified joint resolve, with the pin rewrites its final round authorized.
struct SettledResolve {
    content: String,
    authorized: AuthorizedPins,
}

/// One joint-resolve round's settled lock and the write-set state it left, the postimage a
/// rejection restores from.
struct Round {
    settled: SettledResolve,
    postimage: cooldown_core::ProjectMutationState,
}

/// One importer-filtered exact-pin update, as [`NodeLock::whole_graph_args`] renders it.
struct JointUpdate<'a> {
    pins: &'a [(String, String)],
    filters: &'a [String],
    window_minutes: Option<i64>,
    minimum_age_excludes: &'a [String],
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
    Ok(newest_versions(L::parse(content)?))
}

/// [`locked_versions`] without the releases only npm aliases install
/// ([`NodeLock::alias_only_versions`]), the newest-copy evidence a landing is judged on.
///
/// An alias target is a copy of no declaration of its name, so it can neither prove that a move of
/// the name landed nor, by being newer, hide the genuine copy that did.
/// The evaluation and reporting views keep it, since it is still a release the lock resolves.
fn landing_versions<L: NodeLock>(content: &str) -> Result<HashMap<String, String>> {
    let alias_only_versions = L::alias_only_versions(content);
    Ok(newest_versions(L::parse(content)?.into_iter().filter(
        |NameVersion { name, version, .. }| !alias_only_versions.contains(name, version),
    )))
}

/// The newest version of each name among `entries`.
fn newest_versions(entries: impl IntoIterator<Item = NameVersion>) -> HashMap<String, String> {
    let mut versions: HashMap<String, String> = HashMap::new();
    for NameVersion { name, version, .. } in entries {
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

/// The root manifest's declared names, the direct set of a lock without per-member attribution
/// (npm v1, an unsupported `bun.lock`, or a yarn classic lock whose workspace members cannot all
/// be discovered).
struct ManifestDirect<'a> {
    /// Every name the root manifest declares.
    names: HashSet<String>,
    /// The releases only npm aliases install ([`NodeLock::alias_only_versions`]).
    alias_only: &'a AliasOnlyVersions,
}

impl ManifestDirect<'_> {
    /// Whether the root's declaration of `name` claims the copy at `version`.
    ///
    /// The names are read without versions, so a version only an npm alias installs
    /// (`"foo": "npm:other@2.0.0"`) must be held out explicitly: crediting it to the root's own
    /// declaration of `other` would let a move of the alias target rewrite that unrelated range,
    /// and its declared bound would fall back to the root manifest too.
    /// A registry copy of the same version at `other`'s own key gives the release back that claim,
    /// while a local or workspace source of the same name and version is another package.
    fn claims(&self, name: &str, version: &str) -> bool {
        self.names.contains(name) && !self.alias_only.contains(name, version)
    }
}

/// The declared bound of `name` from the manifests of those `members` that `index` lets an edit
/// move the copy at `version` through ([`MemberIndex::editable_members_for`]).
///
/// A row whose every owner binds it through a source of its own, such as a git dependency, has no
/// registry declaration to bound it, so it has no bound rather than the root manifest's, which
/// only a row without members falls back to.
fn editable_declared_bound(
    root: &Utf8Path,
    index: &MemberIndex,
    members: &[MemberRef],
    name: &str,
    version: &str,
) -> Result<Option<String>> {
    let editable = index.editable_members_for(name, version);
    let bounding: Vec<MemberRef> = members
        .iter()
        .filter(|member| editable.contains(&member.path))
        .cloned()
        .collect();
    if bounding.is_empty() && !members.is_empty() {
        return Ok(None);
    }
    manifest::declared_bound(root, &bounding, name)
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

/// The JavaScript/TypeScript implementation of the
/// [`Tool`](cooldown_core::Tool) port, generic over a [`NodeLock`].
///
/// It detects projects by their lockfile, reads the resolved graph from that lock, recovers
/// direct/transitive classification from lock importer data or the root `package.json`, and resolves
/// publish times from the shared [`NpmRegistry`]. npm has no native cooldown config, so
/// [`native_policy`] is always empty.
///
/// An apply never commits a lock that newly moves a dependency off an exact pin some manifest
/// declares in an install field, whether or not the plan names that package, unless the pin is the
/// active change's own and `--rewrite` or `fix --downgrade-pinned` authorized rewriting it.
/// The guarantee needs per-member resolution evidence: pnpm's importers, npm's v2/v3 install tree,
/// a supported `bun.lock`'s package keys, or a yarn classic lock's selector keys read through its
/// workspace manifests.
/// An npm v1 lock records none, so this guard protects none of its exact pins.
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
    /// Publish times already trusted during planning, reused by native exemption persistence.
    publish_times:
        tokio::sync::Mutex<HashMap<PackageId, HashMap<Version, Option<jiff::Timestamp>>>>,
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
            publish_times: tokio::sync::Mutex::new(HashMap::new()),
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

    /// Drives the package manager through `cmd` instead of the default driver, so an adapter can
    /// set its environment and a test can substitute a scripted fake.
    #[must_use]
    pub(crate) fn with_driver(mut self, cmd: NodeCmd) -> Self {
        self.cmd = cmd;
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

/// Reads the root manifest, every manifest this plan could rewrite, and every other manifest the
/// lock names, the baseline exact pins the settled resolve must leave in place
/// ([`ProtectedPins`]) whatever the plan names.
/// A manager whose member evidence reads the workspace manifests
/// ([`NodeLock::WORKSPACE_MANIFESTS`]) adds every member manifest its discovery names, whether or
/// not a lock exists yet, and records which members discovery found, and for Yarn Berry the
/// configuration their bindings depend on, so no trial re-derives them from a project it may have
/// changed.
///
/// The manifests are read, not journaled: cooldown writes none of the extra ones, so a member
/// manifest that is a symbolic or hard link must not block the mutation the way it blocks a
/// journaled write.
fn baseline<L: NodeLock>(project: &Project, plan: &Plan) -> Result<MutationBaseline> {
    let mut rels: Vec<Utf8PathBuf> = vec![Utf8PathBuf::from("package.json")];
    for change in &plan.changes {
        rels.extend(manifest::manifest_rels(&change.members));
    }
    let members: Vec<MemberRef> = locked_member_paths::<L>(project)?
        .into_iter()
        .map(|path| MemberRef {
            name: path.clone(),
            path,
        })
        .collect();
    rels.extend(manifest::manifest_rels(&members));
    let discovery = L::WORKSPACE_MANIFESTS
        .map(|dialect| manifest::workspaces::BaselineDiscovery::run(&project.root, dialect));
    if let Some(discovery) = &discovery {
        rels.extend(discovery.manifests.iter().cloned());
    }
    let mut baseline = MutationBaseline::read(&project.root, rels)?;
    if let Some(discovery) = discovery {
        discovery.record(&mut baseline);
    }
    Ok(baseline)
}

/// The workspace manifests `L`'s member evidence reads, from disk, or `None` for a manager whose
/// lock records its members' declarations itself ([`NodeLock::WORKSPACE_MANIFESTS`]).
fn workspace_manifests<L: NodeLock>(project: &Project) -> Option<WorkspaceManifests> {
    L::WORKSPACE_MANIFESTS.map(|dialect| WorkspaceManifests::read(&project.root, dialect))
}

/// Every workspace member path the on-disk lock names, sorted; empty without a lock.
///
/// A present lock that cannot be read is an error.
/// One that does not parse names no member, and the apply's strict parse reports it.
fn locked_member_paths<L: NodeLock>(project: &Project) -> Result<BTreeSet<String>> {
    let path = project.root.join(L::LOCKFILE);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(err) => return Err(CoreError::Filesystem(format!("{path}: {err}"))),
    };
    let mut paths: BTreeSet<String> = L::member_paths(&content).into_iter().collect();
    paths.extend(L::member_sources(&content).all_paths());
    Ok(paths)
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

/// The lock's content, with the file named in a read failure: the bare `io::Error` says only
/// "No such file or directory", which a report row cannot act on.
fn read_lock<L: NodeLock>(project: &Project) -> Result<String> {
    let path = project.root.join(L::LOCKFILE);
    std::fs::read_to_string(&path)
        .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))
}

#[async_trait]
impl<L: NodeLock> ToolRead for NpmTool<L> {
    fn id(&self) -> ToolId {
        L::ID
    }

    async fn declarations(&self, project: &Project, name: &str) -> Result<Vec<Declaration>> {
        let content = read_lock::<L>(project)?;
        let workspace = workspace_manifests::<L>(project);
        let index = L::member_sources_in(&content, workspace.as_ref(), &HashSet::new());
        // Yarn classic records no importer entry, so a member's resolved copy is the one its
        // selector identifies, fixed in place or not; every other lock's declarations keep their
        // importer entries alone.
        let selectors = match L::member_attribution_in(&content, workspace.as_ref()) {
            MemberAttribution::Selectors(bindings) => Some(bindings),
            _ => None,
        };
        // Every importer the lock lists, or for yarn classic every workspace member, whatever it
        // declares, so a manifest-only declaration (a peer pnpm did not auto-install) is still
        // found; a single-package lock lists no importers, and its one manifest is the root's.
        let mut paths: BTreeSet<String> = L::member_paths_in(&content, workspace.as_ref())
            .into_iter()
            .collect();
        paths.extend(index.all_paths());
        if paths.is_empty() {
            paths.insert(".".to_string());
        }
        let names = member_names(&project.root, &paths.iter().cloned().collect());
        let mut declarations = Vec::new();
        for path in paths {
            let entry = index
                .entries_of(&path)
                .get(name)
                .map(|entry| (*entry).clone());
            let declared = manifest::member_declaration(&project.root, &path, name)?;
            let member = MemberRef {
                name: names.get(&path).cloned().unwrap_or_else(|| path.clone()),
                path,
            };
            match (declared, entry) {
                (None, None) => {}
                // The manifest says how the member declares the name, one row per distinct range:
                // the lock records a single specifier per importer, so a caret in `dependencies`
                // beside an exact pin in `devDependencies` would otherwise show as the caret alone.
                (Some(declared), entry) => {
                    let resolved = entry.map(|entry| entry.version).or_else(|| {
                        selectors
                            .as_ref()?
                            .bound_version(&member.path, name)
                            .map(str::to_string)
                    });
                    for manifest::RangeFields { range, fields } in declared.by_range() {
                        declarations.push(Declaration {
                            member: member.clone(),
                            range: Some(range),
                            resolved: resolved.clone(),
                            fields: fields.iter().map(ToString::to_string).collect(),
                        });
                    }
                }
                // Only the lock records the declaration.
                (None, Some(entry)) => declarations.push(Declaration {
                    member,
                    range: entry.specifier,
                    resolved: Some(entry.version),
                    fields: Vec::new(),
                }),
            }
        }
        Ok(declarations)
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
        let content = read_lock::<L>(project)?;
        let excluded: HashSet<String> = excluded.iter().map(|member| member.path.clone()).collect();
        let workspace = workspace_manifests::<L>(project);
        let index = L::member_sources_in(&content, workspace.as_ref(), &excluded);
        for dep in deps
            .iter_mut()
            .filter(|dep| dep.direct && !dep.members.is_empty())
        {
            let name = dep.package.name.as_str();
            let version = dep.current.as_str();
            dep.pinned = index.is_exact_pinned(name, version);
            dep.declared_bound =
                editable_declared_bound(&project.root, &index, &dep.members, name, version)?;
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
            honors_generated_members: false,
        }
    }

    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        // The lockfile sits at the workspace root; nested `package.json`s share it (no nested lock).
        ProjectMarker {
            marker: L::LOCKFILE,
            manifest: "package.json",
            alternate_manifests: &[],
            workspace_root: true,
        }
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
        let content = read_lock::<L>(project)?;
        let entries = L::parse(&content)?;
        // Which workspace member(s) own each resolved copy, for source attribution; empty for lock
        // formats without per-member data (npm v1, an unsupported bun.lock, a yarn classic lock
        // whose workspace members cannot all be discovered).
        // npm's index attributes a copy only to the members whose own install-tree lookup binds
        // it, so a nested duplicate of a declared name stays transitive, and yarn's only to the
        // members whose own declaration identifies it, a copy no edit may move included.
        // Member paths are resolved to package names once, by reading each member's
        // `package.json`.
        let workspace = workspace_manifests::<L>(project);
        let member_index = L::member_sources_in(&content, workspace.as_ref(), &HashSet::new());
        let member_names = member_names(&project.root, &member_index.all_paths());
        // Direct-ness comes from the same importer data as attribution: a dependency is direct iff an
        // importer declares it. For pnpm this is version-exact, so a name declared at one version but
        // only pulled in transitively at another (a second copy in the graph) is split correctly —
        // the transitive copy is not reported as a direct dependency with a blank source. Lock
        // formats without importer data fall back to the root `package.json`'s declared names.
        // npm's v2/v3 index already gives an alias's release no owner, since a lookup stopping at
        // an alias slot binds no copy of either name, while a member's own copy from any source
        // keeps its owner even when an alias installs a registry release of the same version.
        let alias_only = L::alias_only_versions(&content);
        let manifest_direct = if member_index.is_authoritative() {
            None
        } else {
            Some(ManifestDirect {
                names: manifest::direct_names(&project.manifest)?,
                alias_only: &alias_only,
            })
        };

        // Advisory identity needs positive origin evidence: the lock entry's own record — npm's and
        // yarn's `resolved` URL naming the public npm registry, or pnpm's registry-only resolution
        // shape and Yarn Berry's binding-free npm locator, whose registry the feed-time query then
        // has to name (see `confirm_advisory_identities`).
        // A format without a per-entry record (bun) grants nothing; the config layers can only veto
        // a granted identity, never substitute for the record.
        // A manager that never reads npm's configuration (Yarn Berry) gets no veto from it.
        let registry_overrides = if L::NPMRC_ROUTING {
            crate::npmrc::RegistryOverrides::read(&project.root, L::NATIVE_MIN_AGE_FILE)
        } else {
            crate::npmrc::RegistryOverrides::none()
        };
        let mut seen: HashMap<(String, String), usize> = HashMap::new();
        let mut deps: Vec<Dependency> = Vec::new();
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
            // On a v2/v3 lock a copy is owned only by the members whose own lookup binds it, so one
            // nested under an unrelated dependency gives a declaration of its name no claim.
            let member_paths = member_index.members_for(&name, &version);
            // A name some member declares through a binding the lock does not prove has no owner,
            // and every copy of it is direct, so the unproven declaration never reads as a
            // transitive one; a release only aliases install is no copy of it.
            let unattributed =
                member_index.is_unattributed(&name) && !alias_only.contains(&name, &version);
            let is_direct = match &manifest_direct {
                Some(fallback) => fallback.claims(&name, &version),
                None => unattributed || !member_paths.is_empty(),
            };
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            let advisory_identity =
                crate::npmrc::advisory_identity(&name, &origin, &registry_overrides);
            // A name can resolve to the same version via several paths, which share one row.
            // Any of them proving the release came from the public registry grants its identity,
            // whichever the lock lists first.
            let release = (name.clone(), version.clone());
            if let Some(&row) = seen.get(&release) {
                if let Some(dep) = deps.get_mut(row)
                    && dep.advisory_identity.is_none()
                {
                    dep.advisory_identity = advisory_identity;
                }
                continue;
            }
            seen.insert(release, deps.len());
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
                editable_declared_bound(&project.root, &member_index, &members, &name, &version)?
            } else {
                None
            };
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

    /// A manager whose members are attributed through the workspace manifests
    /// ([`NodeLock::WORKSPACE_MANIFESTS`]) warns when that evidence is lost: when discovery
    /// cannot establish every member, or the lock is one the manager's own reader would refuse.
    /// The warning names what is lost.
    /// Yarn classic's rows then fall back to the root manifest's names, so no member owns a copy, a
    /// landing is judged by the name's newest locked copy, and no exact manifest pin is protected
    /// from another candidate's resolve.
    /// Yarn Berry's rows count every copy of a name a workspace declares as direct, owned by no
    /// member, so no landing is proven and no exact pin is protected.
    /// A refused lock whose rows cannot be read either has nothing to fall back to, so it is the
    /// error [`NodeLock::parse`] returns instead, as a Yarn classic lock always is, since both its
    /// reads share Yarn's grammar.
    ///
    /// A Yarn Berry lock whose workspaces are known can still leave single declarations unproven
    /// ([`YarnBindings::unattributed`](crate::lock::YarnBindings::unattributed)), which one
    /// warning lists with why.
    async fn manifest_notices(&self, project: &Project) -> Result<Vec<Diagnostic>> {
        let (Some(dialect), Some(workspace)) =
            (L::WORKSPACE_MANIFESTS, workspace_manifests::<L>(project))
        else {
            return Ok(Vec::new());
        };
        let (kind, cause) = if let Some(reason) = workspace.unavailable_reason() {
            let cause = match dialect {
                WorkspaceDialect::Classic => {
                    format!("its workspace members could not all be discovered ({reason})")
                }
                WorkspaceDialect::Berry => format!(
                    "how Yarn binds its workspace members' declarations could not be established ({reason})"
                ),
            };
            (DiagnosticKind::Config, cause)
        } else {
            let content = read_lock::<L>(project)?;
            match L::member_attribution_in(&content, Some(&workspace)) {
                MemberAttribution::Unknown => {
                    // A lock whose rows cannot be read either has no rows to fall back to.
                    L::parse(&content)?;
                    (
                        DiagnosticKind::LockfileUnreadable,
                        format!("{} is no lock {} itself would read", L::LOCKFILE, L::BIN),
                    )
                }
                MemberAttribution::Selectors(bindings) => {
                    return Ok(unattributed_notice(&bindings).into_iter().collect());
                }
                MemberAttribution::Importers(_)
                | MemberAttribution::InstallTree(_)
                | MemberAttribution::PackageKeys(_) => return Ok(Vec::new()),
            }
        };
        let lost = match dialect {
            WorkspaceDialect::Classic => {
                "the rows follow the root package.json alone, a landing is judged by the name's newest locked copy, and no member's exact manifest pin is protected from another candidate's resolve"
            }
            WorkspaceDialect::Berry => {
                "every locked copy of a name a workspace declares is reported as a direct dependency of no member, no landing is proven, and no member's exact manifest pin is protected from another candidate's resolve"
            }
        };
        Ok(vec![
            Diagnostic::new(
                kind,
                format!(
                    "{cause}, so cooldown attributes no dependency to a workspace member: {lost}"
                ),
            )
            .with_path("package.json"),
        ])
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
            .lock_report(
                &project.root,
                &args,
                L::LOCKFILE,
                L::verify_refusal_is_stale,
            )
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
        self.publish_times
            .lock()
            .await
            .entry(dep.package.clone())
            .or_default()
            .extend(
                packument
                    .releases
                    .iter()
                    .map(|release| (release.version.clone(), release.published_at)),
            );
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
        self.publish_times
            .lock()
            .await
            .entry(dep.package.clone())
            .or_default()
            .insert(dep.current.clone(), time);
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
/// for npm's index, which records no per-importer versions and keeps its newest-copy judgment); a
/// converged re-run still moves neither.
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
/// member on its selector's binding (yarn classic, read through `workspace`, the manifests
/// cooldown's edits left), its package-key binding (bun), its member-scoped entry (pnpm), or its
/// physical install-tree instance when the lock records layout (npm), and by the name's newest
/// copy only as the last resort, for a change without declaring members, a lock without any such
/// record, or a bun member that declares the name itself while the lock leaves its copy
/// undetermined.
///
/// Yarn classic has no such fallback: every declaring member must bind the target through its
/// own selector, so a member whose binding the lock does not prove, or one left behind while a
/// sibling's copy moved, never lands the change, and neither does any member when the selectors
/// cannot be read at all, without `workspace`, with its members undiscovered, or with a lock
/// Yarn's own reader would refuse.
///
/// A recorded install tree is authoritative for every declaring member: each must bind a registry
/// copy of the name itself at the target
/// ([`member_registry_version`](crate::lock::InstallPaths::member_registry_version)), one no
/// declaration marks as a remote tarball ([`MemberIndex::non_registry_owners`]).
/// A member bound to nothing, to a link, a git source, or a local or remote tarball, or to an npm
/// alias that installs another package at the name's slot proves no landing, even when that
/// slot's version equals the target or a genuine copy at the target sits elsewhere in the tree.
/// The newest-copy fallback likewise leaves out releases only npm aliases install
/// ([`landing_versions`]).
///
/// A successful install command is not proof: `npm install <name>@<version> --before=<cutoff>`
/// exits 0 yet lands the newest pre-cutoff version when the requested one is newer than the
/// cutoff, so the landing must be read back from the lock.
fn exact_target_reached<L: NodeLock>(
    project: &Project,
    change: &Change,
    workspace: Option<&WorkspaceManifests>,
) -> Result<bool> {
    let content = read_lock::<L>(project)?;
    let newest = landing_versions::<L>(&content)?;
    let target = change.to.as_str();
    let newest_reached = newest
        .get(&change.package.name)
        .is_some_and(|version| version == target);
    if change.members.is_empty() {
        return Ok(newest_reached);
    }
    let name = change.package.name.as_str();
    match L::member_attribution_in(&content, workspace) {
        // A yarn classic member lands only through the copy its own selector binds.
        MemberAttribution::Selectors(bindings) => {
            return Ok(change.members.iter().all(|member| {
                bindings.resolution(&member.path, name) == MemberResolution::Resolved(target)
            }));
        }
        // Bun's attribution judges every member with a known binding on that binding, so another
        // member's newer copy never stands in for it.
        // Only a member that declares the name itself while the lock leaves its copy
        // undetermined, such as one that also declares the name as a peer, falls back to the
        // name's newest copy, the one evidence there is for it.
        // A foreign or local binding is no copy of the name, so no copy elsewhere lands it.
        MemberAttribution::PackageKeys(bindings) => {
            return Ok(change.members.iter().all(|member| {
                match bindings.resolution(&member.path, name) {
                    MemberResolution::Resolved(version) => version == target,
                    MemberResolution::Missing | MemberResolution::Unproven => false,
                    MemberResolution::Unknown => {
                        newest_reached && bindings.is_uncertain(&member.path, name)
                    }
                }
            }));
        }
        // A manager whose members are attributed only through their manifests' declarations has
        // no other evidence for a named member, so without it no landing is proven.
        MemberAttribution::Unknown if L::WORKSPACE_MANIFESTS.is_some() => return Ok(false),
        MemberAttribution::Importers(_)
        | MemberAttribution::InstallTree(_)
        | MemberAttribution::Unknown => {}
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
    // npm's index records no per-importer version, but its lock records the physical install
    // tree: judge each declaring member on the registry copy its own lookup binds — the binding
    // that made the member own the copy at planning — so a newer duplicate nested under another
    // dependent cannot mask a landed root copy.
    // The newest-copy fallback below would read the duplicate, roll the genuine landing back as a
    // conflict, and do so again on every future run.
    if let Some(paths) = L::install_paths(&content) {
        let non_registry = members.non_registry_owners(name, target);
        return Ok(change.members.iter().all(|member| {
            paths.member_registry_version(&member.path, name) == Some(target)
                && !non_registry.contains(&member.path)
        }));
    }
    Ok(newest_reached)
}

/// What the per-package apply does with one candidate, decided before anything is written.
enum CandidateDecision {
    /// Land it through this transaction, judging the settled lock against these pin rewrites and
    /// reading its selectors through these manifest edits.
    Land {
        landing: CandidateLanding,
        authorized: AuthorizedPins,
        edits: ManifestEdits,
    },
    /// Skip it with this row.
    Skip(Skipped),
}

/// What the baseline lock of a per-package apply says about the copies its candidates move.
struct LandingBaseline {
    /// The releases only npm aliases install ([`NodeLock::alias_only_versions`]).
    alias_only: AliasOnlyVersions,
    /// The members owning each copy, and which of them bind it through a source of their own.
    members: MemberIndex,
}

impl LandingBaseline {
    /// The baseline of `lock`, read through the baseline `workspace` manifests where `L` needs
    /// them; an absent lock records no copy.
    fn of<L: NodeLock>(lock: Option<&str>, workspace: Option<&WorkspaceManifests>) -> Self {
        LandingBaseline {
            alias_only: lock.map(L::alias_only_versions).unwrap_or_default(),
            members: lock
                .map(|lock| L::member_sources_in(lock, workspace, &HashSet::new()))
                .unwrap_or_default(),
        }
    }
}

/// The newest locked version of each name in the lock the landings of a per-package apply left.
///
/// # Errors
///
/// Returns a [`CoreError`] when the lock cannot be read or parsed.
fn settled_versions<L: NodeLock>(project: &Project) -> Result<HashMap<String, String>> {
    let after_content = match std::fs::read_to_string(project.root.join(L::LOCKFILE)) {
        Ok(content) => Some(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    // This is fresh post-install content the driver just wrote, so the earlier strict reads
    // prove nothing about it: a failed parse must fail the batch rather than diff against an
    // empty map that hides every collateral move. Only an absent lock diffs as empty.
    versions_in::<L>(after_content.as_deref())
}

/// The newest locked version of each name in the lock `content`, or none without a lock.
///
/// # Errors
///
/// Returns a [`CoreError`] when the lock cannot be parsed.
fn versions_in<L: NodeLock>(content: Option<&str>) -> Result<HashMap<String, String>> {
    Ok(content
        .map(locked_versions::<L>)
        .transpose()?
        .unwrap_or_default())
}

/// The row skipping `change` for `reason`, which names the package itself and needs no detail.
fn own_row(change: &Change, reason: SkipReason) -> Skipped {
    Skipped {
        change: change.clone(),
        reason,
        offending: Some(change.package.clone()),
        detail: None,
    }
}

/// The row `runner` holds `change` with before its landing runs, or `None` to land it.
///
/// A held candidate is restored from `authorized`, the state cooldown's own edits left before the
/// hold read anything.
/// The row's detail is redacted, since a runner quotes declarations and tool output, which may
/// carry registry credentials.
fn runner_hold<L: NodeLock>(
    runner: &dyn CandidateRunner,
    project: &Project,
    plan: &Plan,
    change: &Change,
    candidate_journal: &ProjectMutationJournal,
    authorized: &ProjectMutationState,
) -> Result<Option<Skipped>> {
    let lock = journaled_lock::<L>(candidate_journal);
    let Some(mut held) = runner.hold(project, plan, change, lock)? else {
        return Ok(None);
    };
    restore_after_owned_step(candidate_journal, authorized)?;
    held.detail = held.detail.as_deref().map(redact::url_secrets);
    Ok(Some(held))
}

/// The row skipping `change`, whose landing failed with the candidate's own `error`, with the
/// detail `runner` gives it, redacted as a hold's is ([`runner_hold`]).
///
/// # Errors
///
/// Returns `error` itself when it is a local environment failure.
fn failed_landing_row(
    runner: &dyn CandidateRunner,
    change: &Change,
    error: CoreError,
) -> Result<Skipped> {
    let detail = runner
        .skip_detail(&error)
        .as_deref()
        .map(redact::url_secrets);
    let mut skipped = skipped_on_apply_error(change, error)?;
    skipped.detail = skipped.detail.or(detail);
    Ok(skipped)
}

/// Plans one per-package candidate's landing ([`candidate_landing`]), or the row that skips it.
///
/// A release the baseline lock installs only through npm aliases
/// ([`NodeLock::alias_only_versions`]) is not eligible.
/// It has no declaration of its own, so the manifest fallback would find the root's unrelated
/// declaration of the target's name and rewrite it, bypassing the pin hold, which guards only
/// direct changes.
///
/// Any other transitive candidate is rejected too, before any manifest is inspected.
/// No member owns its copy, so with its empty members the manifest fallback would reach the root's
/// declaration of the name, which binds another copy, and the landing moves a copy only through a
/// declaration of its own.
/// A direct candidate without members, which a lock without per-member attribution plans, keeps
/// the root manifest as its owner.
///
/// A direct candidate is not eligible either when a member binds its copy in a way no edit of its
/// declaration is proven to move ([`MemberIndex::fixed_owners`]): a source of its own, a git
/// dependency or a local or remote tarball, and for yarn classic a local path, a selector the root
/// `resolutions` can rewrite, a name a workspace package carries, an origin that is no registry
/// tarball, or a registry reference to the name itself (`"chalk": "npm:chalk@^4.1.0"`), which no
/// widen rewrites.
/// The member declares it directly, but npm's exact pin would replace that source with a registry
/// release, and no range the widen writes is proven to land one either.
/// The row names each such member with its reason.
/// That check runs first: an alias can install a registry release of the same version beside
/// such a copy, and the copy still has its owner, so the row names the source rather than the
/// alias.
///
/// A candidate whose name some member declares through a binding the lock does not prove
/// ([`MemberIndex::is_unattributed`]) is held before all of these: its row is direct but owned by
/// no member, so neither a declaration to edit nor a member to judge the landing at is known.
fn decide_candidate<L: NodeLock>(
    project: &Project,
    plan: &Plan,
    change: &Change,
    baseline: &LandingBaseline,
) -> Result<CandidateDecision> {
    let skip = |reason, detail| {
        CandidateDecision::Skip(Skipped {
            change: change.clone(),
            reason,
            offending: Some(change.package.clone()),
            detail,
        })
    };
    let name = &change.package.name;
    if baseline.members.is_unattributed(name) {
        let detail = format!(
            "cooldown cannot prove which copy of {name} its declaring workspace members bind, so no declaration is known to move {name}@{}",
            change.from
        );
        return Ok(skip(SkipReason::NotEligible, Some(detail)));
    }
    let fixed = baseline.members.fixed_owners(name, change.from.as_str());
    let mut held: BTreeMap<FixedBinding, Vec<String>> = BTreeMap::new();
    for member in &change.members {
        for FixedOwner { reason, .. } in fixed.iter().filter(|owner| owner.member == member.path) {
            held.entry(*reason).or_default().push(member.name.clone());
        }
    }
    if !held.is_empty() {
        let copy = format!("{name}@{}", change.from);
        let detail = held
            .iter()
            .map(|(reason, members)| fixed_binding_detail(*reason, &copy, name, members))
            .collect::<Vec<_>>()
            .join("; ");
        return Ok(skip(SkipReason::NotEligible, Some(detail)));
    }
    if baseline.alias_only.contains(name, change.from.as_str()) {
        let detail = format!(
            "{name}@{} is installed only through an npm alias, so no declaration of {name} owns it",
            change.from
        );
        return Ok(skip(SkipReason::NotEligible, Some(detail)));
    }
    if !change.direct {
        let detail = format!(
            "{name}@{} is a transitive copy that no member's declaration binds, so no manifest edit may move it",
            change.from
        );
        return Ok(skip(SkipReason::NotEligible, Some(detail)));
    }
    Ok(
        match candidate_landing::<L>(project, change, plan.rewrite, plan.rewrite_pins)? {
            LandingPlan::Land {
                landing,
                authorized,
                edits,
            } => CandidateDecision::Land {
                landing,
                authorized,
                edits,
            },
            LandingPlan::NotEligible => skip(SkipReason::NotEligible, None),
            LandingPlan::PinHeld { detail } => skip(SkipReason::PinHeld, Some(detail)),
        },
    )
}

/// The warning listing every name a workspace member declares through a binding the lock does not
/// prove, each with those members and why, or `None` when every binding is proven.
fn unattributed_notice(bindings: &YarnBindings) -> Option<Diagnostic> {
    let names = bindings.unattributed();
    if names.is_empty() {
        return None;
    }
    let listed = names
        .iter()
        .flat_map(|UnattributedName { name, declarations }| {
            let mut by_reason: BTreeMap<Unproven, Vec<&str>> = BTreeMap::new();
            for UnprovenDeclaration { member, reason } in declarations {
                by_reason.entry(*reason).or_default().push(member);
            }
            by_reason.into_iter().map(move |(reason, members)| {
                format!("{name} in {} ({})", members.join(", "), reason.describe())
            })
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(
        Diagnostic::new(
            DiagnosticKind::Config,
            format!(
                "cooldown cannot prove which locked copy these workspace declarations bind: {listed}; every locked copy of these names is reported as a direct dependency of no member, none is landed, and their exact manifest pins are not protected"
            ),
        )
        .with_path("package.json"),
    )
}

/// Why `copy` (`name@version`) is not eligible for `members`, which bind it as `reason` says.
fn fixed_binding_detail(
    reason: FixedBinding,
    copy: &str,
    name: &str,
    members: &[String],
) -> String {
    let members = list_members(members);
    match reason {
        FixedBinding::OwnSource => format!(
            "{copy} is installed for {members} from a source of its own (git, or a local or remote tarball), so no registry release may replace it through that declaration"
        ),
        FixedBinding::LocalPath => format!(
            "{copy} is installed for {members} from a local path (`file:` or `link:`), so no registry release may replace it through that declaration"
        ),
        FixedBinding::Resolutions => format!(
            "{copy} is resolved for {members} under the root `resolutions` field, which overrides any range a manifest edit writes; edit the resolution to move it"
        ),
        FixedBinding::WorkspaceName => format!(
            "{name} is also a workspace package's name, which Yarn links in place of a registry release whenever its version satisfies the range, so no manifest edit is proven to move {copy} for {members}"
        ),
        FixedBinding::UnrecognizedOrigin => format!(
            "{copy} is locked for {members} from a URL that is no registry tarball of it, so no manifest edit is proven to land a registry release in its place"
        ),
        FixedBinding::SelfAlias => format!(
            "{copy} is declared for {members} through an `npm:` or `yarn:` reference to {name} itself, which cooldown never rewrites, so no manifest edit may move it"
        ),
    }
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
/// Importers the run excludes are not part of this contract.
/// The filtered update never names them, but the override repair ([`NpmTool::resolve_round`])
/// re-resolves the whole workspace under temporary overrides for the planned targets.
/// An excluded importer whose declared range intersects the override's key can therefore move to
/// the target, and the settled-lock guard reports or refuses that move ([`report_excluded_moves`]).
struct PartialLanding {
    index: usize,
    /// The in-scope importers that reached the target.
    landed: Vec<String>,
    /// The in-scope importers that did not, with the version each sits at (`None`: no longer
    /// declared).
    stayed: Vec<(MemberRef, Option<String>)>,
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
                version => stayed.push((member.clone(), version.map(str::to_string))),
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
/// An importer that declares the package only in `peerDependencies` is the one structural cause
/// cooldown can name ([`peer_only_remedy`]), so the row then says what would let it move instead of
/// ending at the importer's name.
fn partial_landing_skip<L: NodeLock>(
    root: &Utf8Path,
    change: &Change,
    partial: &PartialLanding,
    after_content: &str,
) -> Result<Skipped> {
    let name = change.package.name.as_str();
    let target = change.to.as_str();
    let stayed = partial
        .stayed
        .iter()
        .map(|(member, version)| {
            format!(
                "{} at {}",
                member.name,
                version.as_deref().unwrap_or("no resolved copy")
            )
        })
        .collect::<Vec<_>>();
    let offender = peer_conflict_blocker(after_content, name).unwrap_or_else(|| name.to_string());
    let mut detail = format!(
        "the resolve landed {target} in {} of {} importers ({}) and left {}; rolled back rather than split {name} across importers",
        partial.landed.len(),
        partial.landed.len() + partial.stayed.len(),
        list_members(&partial.landed),
        list_members(&stayed),
    );
    if let Some(remedy) = peer_only_remedy(root, name, &partial.stayed)? {
        detail.push_str("; ");
        detail.push_str(&remedy);
    }
    Ok(Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(PackageId::new(L::ID, offender, Some(NPM.to_string()))),
        detail: Some(detail),
    })
}

/// Why a partial landing is a dead end, when cooldown can tell: the importers among `stayed` that
/// declare `name` only in `peerDependencies`, each group with the change that lets it move.
///
/// pnpm auto-installs a workspace package's own peer and records it in that importer's lock entry,
/// so the importer counts as declaring the name — but `pnpm update` rewrites install fields only,
/// so its copy never advances while every sibling's does (verified against pnpm 11: an importer
/// with `chalk` only under `peerDependencies` kept 4.1.0 while the joint update landed 4.1.2 in
/// the importer beside it).
/// `--rewrite` does not help either, since the declared range already admits the target, so
/// without this sentence the row names the importer and no action.
/// A library keeps its published contract and adds a `devDependencies` entry for the install; an
/// application (`private: true`) publishes a contract to no one and moves the entry to
/// `dependencies`.
/// `None` when no stayed importer is peer-only.
fn peer_only_remedy(
    root: &Utf8Path,
    name: &str,
    stayed: &[(MemberRef, Option<String>)],
) -> Result<Option<String>> {
    let mut libraries = Vec::new();
    let mut applications = Vec::new();
    for (member, _) in stayed {
        let Some(declaration) = manifest::member_declaration(root, &member.path, name)? else {
            continue;
        };
        if !declaration.is_peer_only() {
            continue;
        }
        if declaration.private {
            applications.push(member.name.clone());
        } else {
            libraries.push(member.name.clone());
        }
    }
    let sentence = |members: &[String], remedy: &str| {
        let declares = if members.len() == 1 {
            "declares"
        } else {
            "declare"
        };
        format!(
            "{} {declares} {name} only in `peerDependencies`, which `pnpm update` cannot advance; {remedy} to let it move",
            list_members(members)
        )
    };
    let mut sentences = Vec::new();
    if !libraries.is_empty() {
        sentences.push(sentence(
            &libraries,
            "declare it in `devDependencies` as well",
        ));
    }
    if !applications.is_empty() {
        sentences.push(sentence(&applications, "move it to `dependencies`"));
    }
    Ok((!sentences.is_empty()).then(|| sentences.join("; ")))
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
/// Whether a deliberate split also gives a gated name a copy the graph did not have is the
/// graph-level guard's judgement, which sees the whole graph; this one returns the names whose
/// split it reported so that guard does not describe the same importer copies twice.
fn report_importer_splits(
    report: &mut ApplyReport,
    before: &MemberIndex,
    after: &MemberIndex,
    excluded: &HashSet<String>,
) -> Result<BTreeSet<String>> {
    let mut reported = BTreeSet::new();
    for split in importer_splits(before, after) {
        if !split.is_deliberate(excluded) {
            return Err(CoreError::UnacceptableResolve(format!(
                "the resolve split a workspace dependency: {}",
                split.describe()
            )));
        }
        report.warnings.push(
            Diagnostic::new(
                DiagnosticKind::DuplicateCopy,
                format!(
                    "{}; the importer(s) still at {} are excluded from this run and were left in place",
                    split.describe(),
                    split.before
                ),
            )
            .with_package(split.name.clone()),
        );
        reported.insert(split.name);
    }
    Ok(reported)
}

/// Reports every name the settled resolve left at more resolved versions across the whole package
/// graph than it had before — counted as distinct versions, whoever holds them, since a runtime
/// with instance-level state breaks on the second copy whether an importer declares it or a
/// dependent's requirement pulls it in.
/// The new copies are attributed ([`NewCopies`]): an importer's own by the importers holding it,
/// one no importer declares by the packages whose own requirement binds it, named from the settled
/// lock's edges so the reader need not grep the `snapshots:` section for the culprit.
/// Such a copy is the resolver's legitimate answer to a range, so it is committed, but a lock that
/// gained a copy must never pass silently — the run's own report names the versions, whatever the
/// newest-copy diff shows; a deliberate excluded-importer split the importer-level guard already
/// described is not described again unless a copy beyond the importers' own appeared.
/// A name `single_copy` gates is refused instead: the batch fails with the copies and their
/// holders or requirers named, and the caller's candidate isolation holds the candidate whose
/// landing added them.
/// A name whose count of versions did not grow is not the run's doing — a copy that moved is still
/// one copy — so it passes; a listed name's standing split is reported so the listing does not
/// read as an enforced invariant while it is not one.
fn report_graph_duplicates<L: NodeLock>(
    report: &mut ApplyReport,
    before_content: &str,
    after_content: &str,
    after_members: &MemberIndex,
    excluded: &HashSet<String>,
    split_reported: &BTreeSet<String>,
    single_copy: &SingleCopyPolicy,
) -> Result<()> {
    let before = resolved_version_lines::<L>(before_content)?;
    let after = resolved_version_lines::<L>(after_content)?;
    let mut names: Vec<&String> = after.keys().collect();
    names.sort();
    // The requirement edges are read once, and only when a duplicate needs its requirer named.
    let mut dependents = None;
    for name in names {
        let Some(now) = after.get(name).filter(|now| now.len() > 1) else {
            continue;
        };
        let was = before.get(name);
        let was_count = was.map_or(0, BTreeSet::len);
        if now.len() <= was_count.max(1) {
            if was_count > 1 && single_copy.names.contains(name) {
                report.warnings.push(
                    Diagnostic::new(
                        DiagnosticKind::DuplicateCopy,
                        format!(
                            "{name} is listed in `[tool.pnpm] single-copy` but was already resolved at several versions before the run ({}); the gate refuses only a copy a resolve adds, so converge it for the listing to hold",
                            was.map(|was| was.iter().cloned().collect::<Vec<_>>().join(", "))
                                .unwrap_or_default()
                        ),
                    )
                    .with_package(name.clone()),
                );
            }
            continue;
        }
        let dependents = dependents.get_or_insert_with(|| L::graph_dependents(after_content));
        let copies = NewCopies::of(name, was, now, after_members, excluded, dependents);
        let described = format!("{name} {}: {}", copies.history(), copies.attribution());
        if let Some(gate) = single_copy.gate_of(name) {
            return Err(CoreError::UnacceptableResolve(copies.refusal(
                name,
                gate,
                single_copy,
                &described,
            )));
        }
        if copies.undeclared.is_empty() && split_reported.contains(name) {
            continue;
        }
        report.warnings.push(
            Diagnostic::new(DiagnosticKind::DuplicateCopy, described).with_package(name.clone()),
        );
    }
    Ok(())
}

/// The copies a settled resolve gave a name beyond what the graph had, attributed for the report
/// (see [`report_graph_duplicates`]).
struct NewCopies<'a> {
    /// The distinct versions before the run, sorted.
    was: Vec<&'a str>,
    /// The distinct versions after the run, sorted.
    now: Vec<&'a str>,
    /// The versions new to the graph that an importer holds, with the importers holding each.
    held: Vec<(&'a str, Vec<String>)>,
    /// The versions no importer declares that the report explains — the new ones, and where the
    /// graph had at most one version before, the one the importers left behind — with the
    /// packages whose own requirement binds each.
    undeclared: Vec<(&'a str, Vec<String>)>,
    /// The versions held only by importers the run excludes and required by no other package,
    /// with those importers: an exclusion asked for them to stay, so a refusal names the way out.
    left_behind: Vec<(&'a str, Vec<String>)>,
    /// The versions held only by excluded importers that a package's own requirement also binds,
    /// with the importers and the requirers: lifting the exclusion alone would not converge them,
    /// so the refusal names the requirer instead of promising that it would.
    also_required: Vec<(&'a str, Vec<String>, Vec<String>)>,
    /// Whether lifting the exclusion on `left_behind`'s importers would leave the name at one
    /// copy: every other version after the run is the one new version the included importers
    /// moved onto. A stray a dependent retains, a version an excluded importer re-resolved onto,
    /// a standing split's other copies, or two included importers on two new versions would all
    /// survive, so the refusal must not promise it.
    converges_by_inclusion: bool,
}

impl<'a> NewCopies<'a> {
    fn of(
        name: &str,
        was: Option<&'a BTreeSet<String>>,
        now: &'a BTreeSet<String>,
        after_members: &MemberIndex,
        excluded: &HashSet<String>,
        dependents: &HashMap<(String, String), Vec<String>>,
    ) -> Self {
        let was_count = was.map_or(0, BTreeSet::len);
        let importer_held = after_members.resolved_versions_of(name);
        let mut copies = NewCopies {
            was: was
                .map(|was| was.iter().map(String::as_str).collect())
                .unwrap_or_default(),
            now: now.iter().map(String::as_str).collect(),
            held: Vec::new(),
            undeclared: Vec::new(),
            left_behind: Vec::new(),
            also_required: Vec::new(),
            converges_by_inclusion: true,
        };
        let mut included_new = 0;
        for version in now {
            let new = !was.is_some_and(|was| was.contains(version));
            let requirers = dependents
                .get(&(name.to_string(), version.clone()))
                .cloned()
                .unwrap_or_default();
            if importer_held.contains(&version.as_str()) {
                let holders = after_members.members_for(name, version);
                let only_excluded =
                    !holders.is_empty() && holders.iter().all(|member| excluded.contains(member));
                let left_behind = only_excluded && !new && requirers.is_empty();
                // The copy the included importers moved onto is the one everything would
                // converge on; any other version would survive lifting the exclusion.
                copies.converges_by_inclusion &= left_behind || (new && !only_excluded);
                included_new += usize::from(new && !only_excluded);
                if left_behind {
                    copies.left_behind.push((version.as_str(), holders.clone()));
                } else if only_excluded && !requirers.is_empty() {
                    copies
                        .also_required
                        .push((version.as_str(), holders.clone(), requirers));
                }
                if new {
                    copies.held.push((version.as_str(), holders));
                }
            } else {
                copies.converges_by_inclusion = false;
                if new || was_count <= 1 {
                    copies.undeclared.push((version.as_str(), requirers));
                }
            }
        }
        copies.converges_by_inclusion &= included_new == 1;
        copies
    }

    /// How the name's versions changed across the run, the way the report tells it.
    fn history(&self) -> String {
        match self.was.as_slice() {
            [] => format!(
                "was absent from the graph before the run and is at several versions after it ({})",
                self.now.join(", ")
            ),
            [single] => format!(
                "resolved at one version ({single}) across the graph before the run and at several after it ({})",
                self.now.join(", ")
            ),
            was => {
                let added: Vec<&str> = self
                    .now
                    .iter()
                    .copied()
                    .filter(|version| !was.contains(version))
                    .collect();
                format!(
                    "was already at several versions before the run ({}) and gained another after it ({})",
                    was.join(", "),
                    added.join(", ")
                )
            }
        }
    }

    /// Who holds or requires each copy the report explains.
    fn attribution(&self) -> String {
        let held = self
            .held
            .iter()
            .map(|(version, members)| format!("{version} in {}", list_members(members)))
            .collect::<Vec<_>>()
            .join(", ");
        let undeclared = self
            .undeclared
            .iter()
            .map(|(version, requirers)| {
                if requirers.is_empty() {
                    format!("{version} pulled in by another package's requirement")
                } else {
                    format!("required at {version} by {}", list_members(requirers))
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        let copies = match (held.is_empty(), undeclared.is_empty()) {
            (false, true) => format!("held by an importer: {held}"),
            (true, _) => format!("a copy no importer declares, {undeclared}"),
            (false, false) => {
                format!("held by an importer: {held}; a copy no importer declares, {undeclared}")
            }
        };
        let also_required = self
            .also_required
            .iter()
            .map(|(version, holders, requirers)| {
                format!(
                    "{version} in {} is also required by {}",
                    list_members(holders),
                    list_members(requirers)
                )
            })
            .collect::<Vec<_>>();
        if also_required.is_empty() {
            copies
        } else {
            format!("{copies}; {}", also_required.join("; "))
        }
    }

    /// The refusal a gated name gets: a copy left only in an excluded importer, where lifting the
    /// exclusion would converge the copies, names the two ways out — dropping every gate that
    /// applies (the listing, the flag, or both), or lifting the exclusion — since that importer is
    /// never moved by the run and the refusal would otherwise recur.
    fn refusal(
        &self,
        name: &str,
        gate: &str,
        single_copy: &SingleCopyPolicy,
        described: &str,
    ) -> String {
        if !self.left_behind.is_empty() && self.converges_by_inclusion {
            let left = self
                .left_behind
                .iter()
                .map(|(version, members)| format!("{version} in {}", list_members(members)))
                .collect::<Vec<_>>()
                .join(", ");
            let importers: Vec<String> = self
                .left_behind
                .iter()
                .flat_map(|(_, members)| members.iter().cloned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let drop_gate = match (single_copy.names.contains(name), single_copy.every_name) {
                (true, true) => {
                    format!(
                        "drop {name} from the listing and run without `--fail-on-new-duplicate`"
                    )
                }
                (true, false) => format!("drop {name} from the listing"),
                (false, _) => "run without `--fail-on-new-duplicate`".to_string(),
            };
            return format!(
                "the resolve left an excluded importer on a second copy of {name}, which {gate} keeps single-copy: {described}, with {left} left behind; {drop_gate}, or bring {} into the run (lift its exclusion) so the copies converge",
                list_members(&importers)
            );
        }
        let which = if self.was.len() > 1 {
            "another"
        } else {
            "a second"
        };
        // Without the promise, the copy an exclusion left in place is still named: the importer
        // guard's warning that would have named it is discarded with the refused settlement.
        let left = self
            .left_behind
            .iter()
            .map(|(version, members)| {
                format!(
                    "; {version} in {} left behind by its exclusion",
                    list_members(members)
                )
            })
            .collect::<Vec<_>>()
            .concat();
        format!(
            "the resolve added {which} copy of {name}, which {gate} keeps single-copy: {described}{left}"
        )
    }
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
/// The filtered `pnpm update <name>@<target>` re-pins the named package only in the selected
/// importers.
/// The override repair ([`NpmTool::resolve_round`]) is different: it runs an unfiltered install
/// under temporary overrides keyed by each target's major line, and pnpm applies an override to
/// every declaration whose range intersects its key, an excluded importer's included.
/// An excluded importer can therefore move within its own range without cooldown ever naming it or
/// rewriting its manifest.
/// Refusing that move would let an excluded subtree veto every shared upgrade, the very thing
/// exclusions must not do, so it is `re_resolved`: committed and reported.
/// An exact manifest pin it moves is refused separately ([`ProtectedPins`]).
/// A changed declaration (`specifier:`), an entry appearing or disappearing, or a version its own
/// range provably excludes is `drifted`: pnpm never does that on its own, so it is a stale excluded
/// importer refreshed or an override that reached it, and it must not be committed as if the
/// exclusion had been honored.
/// The range judgment is proof-only, like [`MemberIndex::splits_for`]: a range cooldown cannot read
/// (a `||` union, a protocol) or a missing specifier proves nothing, so the move still counts as
/// re-resolved rather than as drift, [`RangeAdmission::Unjudged`].
/// `link:`/`workspace:`/`file:` entries are layout facts no resolve changes and are not compared;
/// one turning into or out of a registry version still shows as an added or removed entry.
#[derive(Default)]
struct ExcludedMoves {
    re_resolved: Vec<ReResolved>,
    drifted: Vec<ExcludedMove>,
}

/// An excluded importer's entry that moved under an unchanged declaration.
struct ReResolved {
    moved: ExcludedMove,
    admission: RangeAdmission,
}

/// Whether the unchanged declaration of a [`ReResolved`] entry provably admits its new version.
#[derive(Clone, Copy)]
enum RangeAdmission {
    /// The declared range admits the new version.
    Proven,
    /// The declaration is missing or a range cooldown cannot judge, so its warning claims no
    /// range membership.
    Unjudged,
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
                    let range_match = to
                        .specifier
                        .as_deref()
                        .map(|specifier| crate::version::range_match(specifier, &to.version));
                    let entry = record(format!("{} → {}", from.version, to.version));
                    let admission = match range_match {
                        Some(crate::version::RangeMatch::Excludes) => {
                            moves.drifted.push(entry);
                            continue;
                        }
                        Some(crate::version::RangeMatch::Matches) => RangeAdmission::Proven,
                        Some(crate::version::RangeMatch::Unknown) | None => {
                            RangeAdmission::Unjudged
                        }
                    };
                    moves.re_resolved.push(ReResolved {
                        moved: entry,
                        admission,
                    });
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
/// The warning says the move stayed within the declared range only when the range provably admits
/// the new version ([`RangeAdmission`]).
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
    for ReResolved { moved, admission } in &moves.re_resolved {
        let described = moved.describe();
        let message = match admission {
            RangeAdmission::Proven => format!(
                "the resolve moved {described} within the range that excluded importer declares; its manifest was not touched"
            ),
            RangeAdmission::Unjudged => format!(
                "the resolve moved {described} under an unchanged declaration cooldown cannot judge against the new version; the excluded manifest was not touched"
            ),
        };
        report.warnings.push(
            Diagnostic::new(DiagnosticKind::ExcludedMoved, message)
                .with_package(moved.name.clone()),
        );
    }
    Ok(())
}

/// The exact manifest pins a settled lock must leave in place, the rewrites the user authorized
/// for the accepted round, and the manifest edits each judged lock was resolved under.
#[derive(Clone, Copy)]
struct PinGuard<'a> {
    protected: &'a ProtectedPins,
    authorized: &'a AuthorizedPins,
    edits: EditStates<'a>,
}

/// The whole-lock guards on the settled resolve, before any candidate is reported applied: no
/// protected exact manifest pin newly violated ([`ProtectedPins`]), excluded importers accounted
/// for ([`report_excluded_moves`]), no in-scope importer split ([`report_importer_splits`]), and
/// every graph-level duplicate the resolve introduced named ([`report_graph_duplicates`]).
/// The pin guard runs first and also without a pre-apply lock, since a lock the run created can
/// still resolve a pinned dependency off its pin.
/// The others need the pre-apply lock as their baseline; without one there is nothing an importer
/// could have moved or split away from, so a lock the run created is otherwise taken as it is.
fn guard_settled_lock<L: NodeLock>(
    report: &mut ApplyReport,
    lock: (Option<&str>, &str),
    members: (Option<&MemberIndex>, &MemberIndex),
    excluded: &HashSet<String>,
    single_copy: &SingleCopyPolicy,
    pins: PinGuard<'_>,
) -> Result<()> {
    let (before_lock, after_lock) = lock;
    let (before_members, after_members) = members;
    pins.protected.refuse_new_violations::<L>(
        before_lock,
        after_lock,
        pins.authorized,
        pins.edits,
    )?;
    let (Some(before_content), Some(before_members)) = (before_lock, before_members) else {
        return Ok(());
    };
    report_excluded_moves(report, before_members, after_members, excluded)?;
    let split_reported = report_importer_splits(report, before_members, after_members, excluded)?;
    report_graph_duplicates::<L>(
        report,
        before_content,
        after_lock,
        after_members,
        excluded,
        &split_reported,
        single_copy,
    )
}

/// One sequential candidate whose target landed, with the journal and postimage that roll it back.
struct LandedCandidate<'a> {
    change: &'a Change,
    journal: &'a ProjectMutationJournal,
    postimage: &'a cooldown_core::ProjectMutationState,
}

/// Accepts a sequential candidate whose target landed, unless its lock newly violates a protected
/// exact pin or a peer contract, and returns whether it was accepted.
///
/// The pins are judged against the candidate's own baseline, the state the earlier accepted
/// landings left, so neither an older violation nor an earlier candidate's authorized rewrite is
/// charged to it, yet it cannot undo either.
/// Each lock is read through the manifest edits it was resolved under ([`PinGuard::edits`]).
/// A pin violation restores the candidate and reports a resolver conflict naming the pin; the peer
/// contracts are then verified by [`settle_landed_candidate`].
fn settle_sequential_landing<L: NodeLock>(
    project: &Project,
    landed: &LandedCandidate<'_>,
    pins: PinGuard<'_>,
    workspace: &[WorkspacePeer],
    peer_baseline: &mut Option<PeerViolations>,
    report: &mut ApplyReport,
) -> Result<bool> {
    let &LandedCandidate {
        change,
        journal,
        postimage,
    } = landed;
    let settled = read_lock::<L>(project)?;
    let violations = pins.protected.violations_between::<L>(
        journaled_lock::<L>(journal),
        &settled,
        pins.authorized,
        pins.edits,
    );
    if !violations.is_empty() {
        restore_after_owned_step(journal, postimage)?;
        report.skipped.push(Skipped {
            change: change.clone(),
            reason: SkipReason::ResolverConflict,
            offending: Some(change.package.clone()),
            detail: Some(crate::pins::refusal(&violations)),
        });
        return Ok(false);
    }
    settle_landed_candidate::<L>(
        project,
        change,
        journal,
        postimage,
        workspace,
        peer_baseline,
        report,
    )
}

/// The importer paths `plan` excludes.
fn excluded_paths(plan: &Plan) -> HashSet<String> {
    plan.excluded_members
        .iter()
        .map(|member| member.path.clone())
        .collect()
}

impl<L: NodeLock> NpmTool<L> {
    /// Applies `mutation` as [`ToolWrite::apply`] does, with `runner` running each per-package
    /// landing transaction.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the mutation belongs to another tool family, and
    /// any error the peer gate, a landing, or a guard on the settled lock raises.
    pub(crate) async fn apply_with_runner(
        &self,
        mutation: &PreparedMutation,
        runner: &dyn CandidateRunner,
    ) -> Result<ApplyReport> {
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
        // A recovery subset shares its operation's baseline, so every trial judges the same pins.
        let protected_pins = ProtectedPins::from_baseline::<L>(mutation.baseline())?;
        let mut report = if L::supports_whole_graph_resolve() {
            self.apply_whole_graph(project, &plan, journal, &evidence, &protected_pins)
                .await?
        } else {
            self.apply_per_package(
                project,
                &plan,
                journal,
                (&evidence.workspace, &protected_pins),
                runner,
            )
            .await?
        };
        report.skipped.extend(peer_held);
        Ok(report)
    }

    /// For each change, moves the lock with a lock-only update or an exact pin around cooldown's
    /// authorized manifest edits, then reports collateral lock movements.
    ///
    /// npm's `--before` constrains the complete resolved tree, so even a per-package command can move
    /// transitives.
    /// Diffing the journaled lock against the final lock keeps those movements visible.
    ///
    /// A landing that newly violates a protected exact manifest pin ([`ProtectedPins`]) is restored
    /// and reported as a resolver conflict naming the pin.
    /// Each candidate is judged against the state the earlier accepted landings left, so only the
    /// candidate that moved the pin is rolled back.
    ///
    /// `runner` runs each landing transaction, and may hold a candidate before its landing runs.
    async fn apply_per_package(
        &self,
        project: &Project,
        plan: &Plan,
        baseline_journal: &ProjectMutationJournal,
        peers: (&[WorkspacePeer], &ProtectedPins),
        runner: &dyn CandidateRunner,
    ) -> Result<ApplyReport> {
        let (peer_workspace, protected_pins) = peers;
        // A project without a journaled lock diffs from an empty baseline; a present but
        // unparsable one is a real error, never an empty graph.
        let before = versions_in::<L>(journaled_lock::<L>(baseline_journal))?;
        let workspace = protected_pins.workspace();
        let landing_baseline =
            LandingBaseline::of::<L>(journaled_lock::<L>(baseline_journal), workspace);
        let mut report = ApplyReport::default();
        let mut violation_baseline: Option<PeerViolations> = None;
        // The pin rewrites of every accepted candidate, which later candidates are judged against
        // instead of the baseline pins they replaced.
        let mut accepted_pins = AuthorizedPins::default();
        // Every manifest edit of the accepted candidates, the selectors each later candidate's
        // starting lock was resolved under; a rolled-back candidate's edits never join them.
        let mut accepted_edits = ManifestEdits::default();
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
            let (landing, candidate_pins, candidate_edits) =
                match decide_candidate::<L>(project, plan, change, &landing_baseline)? {
                    CandidateDecision::Land {
                        landing,
                        authorized,
                        edits,
                    } => (landing, authorized, edits),
                    CandidateDecision::Skip(skipped) => {
                        report.skipped.push(skipped);
                        continue;
                    }
                };
            // The state cooldown's widen left, captured before anything else reads the project, is
            // what a hold or a landing that writes nothing rolls back from.
            let authorized = candidate_journal.capture_state()?;
            if let Some(held) = runner_hold::<L>(
                runner,
                project,
                plan,
                change,
                &candidate_journal,
                &authorized,
            )? {
                report.skipped.push(held);
                continue;
            }
            let attempt = runner
                .land(project, change, &candidate_journal, &landing, &authorized)
                .await?;
            let settled_edits = accepted_edits.union(&candidate_edits);
            let landed_workspace = workspace.map(|workspace| settled_edits.apply_to(workspace));
            match attempt.result {
                Ok(())
                    if exact_target_reached::<L>(project, change, landed_workspace.as_ref())? =>
                {
                    let authorized = accepted_pins.union(&candidate_pins);
                    let landed = LandedCandidate {
                        change,
                        journal: &candidate_journal,
                        postimage: &attempt.postimage,
                    };
                    let pins = PinGuard {
                        protected: protected_pins,
                        authorized: &authorized,
                        edits: EditStates {
                            baseline: &accepted_edits,
                            settled: &settled_edits,
                        },
                    };
                    if settle_sequential_landing::<L>(
                        project,
                        &landed,
                        pins,
                        peer_workspace,
                        &mut violation_baseline,
                        &mut report,
                    )? {
                        accepted_pins = authorized;
                        accepted_edits = settled_edits;
                    }
                }
                Ok(()) => {
                    // The install ran but the target did not land: a dependent's range holds
                    // the package where it is, a ceiling the resolver imposed rather than a
                    // rejection it raised.
                    restore_after_owned_step(&candidate_journal, &attempt.postimage)?;
                    report
                        .skipped
                        .push(own_row(change, SkipReason::GraphCeilingHeld));
                }
                Err(error) if error.is_local_environment_failure() => {
                    restore_after_owned_step(&candidate_journal, &attempt.postimage)?;
                    return Err(error);
                }
                Err(error) => {
                    restore_after_owned_step(&candidate_journal, &attempt.postimage)?;
                    report
                        .skipped
                        .push(failed_landing_row(runner, change, error)?);
                }
            }
        }
        let after = settled_versions::<L>(project)?;
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
        protected_pins: &ProtectedPins,
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
        let (plan, catalog_holds) = catalog_partition::<L>(plan, before_content, &excluded);
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
        let advance = transitive_advance_verdicts::<L>(plan, before_content, &excluded)?;

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
        let SettledResolve {
            content: after_content,
            authorized,
        } = self
            .resolve_and_verify(
                project,
                &JointResolve {
                    plan,
                    journal,
                    multi_version,
                    advance: &advance,
                    window_minutes,
                    workspace,
                    protected_pins,
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
            &plan.single_copy,
            PinGuard {
                protected: protected_pins,
                authorized: &authorized,
                edits: EditStates::NONE,
            },
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
                report.skipped.push(multi_version_hold(
                    change,
                    &after_in_scope,
                    plan,
                    &evidence.split_holds,
                ));
            } else {
                let advanced = matches!(
                    advance.get(&advance_key(change)),
                    Some(TransitiveAdvance::Pin(_))
                );
                report
                    .skipped
                    .push(graph_ceiling_hold::<L>(change, &after_content, advanced)?);
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
    ///
    /// Each round's resolve, repair included, is [`Self::resolve_round`].
    async fn resolve_and_verify(
        &self,
        project: &Project,
        inputs: &JointResolve<'_>,
        rejections: &mut Vec<Skipped>,
    ) -> Result<SettledResolve> {
        let &JointResolve {
            plan,
            journal,
            multi_version,
            workspace,
            ..
        } = inputs;
        let before_content = journaled_lock::<L>(journal);
        let baseline = PeerBaseline::gather::<L>(before_content, workspace);
        let mut active = plan.clone();
        loop {
            let Round {
                settled:
                    SettledResolve {
                        content: after_content,
                        authorized,
                    },
                postimage,
            } = self.resolve_round(project, inputs, &active).await?;
            let current = proven_peer_violations::<L>(&after_content, workspace);
            let peer = plan_peer_rejections(&baseline, &current, &active, multi_version)?;
            let partial =
                partial_landings(&active, multi_version, &L::member_sources(&after_content));
            if peer.is_empty() && partial.is_empty() {
                return Ok(SettledResolve {
                    content: after_content,
                    authorized,
                });
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
                        partial_landing_skip::<L>(&project.root, &change, &landing, &after_content)?
                    }
                });
            }
            restore_after_owned_step(journal, &postimage)?;
            if active.changes.is_empty() {
                // Every candidate was rejected; the restored journal is the result.
                return Ok(SettledResolve {
                    content: before_content.unwrap_or_default().to_string(),
                    authorized: AuthorizedPins::default(),
                });
            }
        }
    }

    /// Runs one round's joint resolve and returns the lock it settled on with the state to restore
    /// it from.
    ///
    /// A starting lock pnpm's native age gate refuses, or a targeted update that floats an entry out
    /// of its declared range, is retried through the override repair from the restored journal.
    /// The repair's install is unfiltered, so its overrides can also reach an excluded importer
    /// whose declared range intersects their key: a move that survives the settlement under an
    /// unchanged declaration is an accepted side effect the settled-lock guard reports
    /// ([`report_excluded_moves`]).
    /// A repair that still leaves a fresh inconsistency fails as a stale lock, unless it moved a
    /// protected exact pin ([`ProtectedPins`]): that refusal names a resolver verdict the caller's
    /// candidate isolation can act on, which a stale lock is not.
    async fn resolve_round(
        &self,
        project: &Project,
        inputs: &JointResolve<'_>,
        active: &Plan,
    ) -> Result<Round> {
        let &JointResolve {
            journal,
            multi_version,
            advance,
            window_minutes,
            protected_pins,
            ..
        } = inputs;
        let before_content = journaled_lock::<L>(journal);
        let resolve_result = self
            .whole_graph_resolve(project, active, multi_version, advance, window_minutes)
            .await;
        // A failed round's records are moot: its edits are restored and nothing is judged.
        let (resolve_result, mut authorized) = match resolve_result {
            Ok(authorized) => (Ok(()), authorized),
            Err(error) => (Err(error), AuthorizedPins::default()),
        };
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
                authorized = self
                    .repair_policy_rejected_graph(
                        project,
                        active,
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

        let mut after_content = read_lock::<L>(project)?;
        if new_lock_inconsistency::<L>(before_content, &after_content).is_some() {
            // pnpm's targeted `update` can float an *unrelated* importer entry out of its
            // declared range: peer unification across linked workspace members pulls a sibling
            // importer's version line into this one (observed on luup5 as `vite: ^6` landing
            // at 7.3.5 while updating `@playwright/test`). The override-based repair engine
            // resolves through a plain install, which provably respects declared ranges, so
            // the same pins are retried through it; only a repair that still leaves a fresh
            // inconsistency is a real stale-lock failure.
            restore_after_owned_step(journal, &resolve.postimage)?;
            authorized = self
                .repair_policy_rejected_graph(
                    project,
                    active,
                    multi_version,
                    advance,
                    window_minutes,
                )
                .await?;
            resolve.postimage = journal.capture_state()?;
            after_content = read_lock::<L>(project)?;
            if let Some(detail) = new_lock_inconsistency::<L>(before_content, &after_content) {
                protected_pins.refuse_new_violations::<L>(
                    before_content,
                    &after_content,
                    &authorized,
                    EditStates::NONE,
                )?;
                return Err(CoreError::StaleLock(detail));
            }
        }
        Ok(Round {
            settled: SettledResolve {
                content: after_content,
                authorized,
            },
            postimage: resolve.postimage,
        })
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
    ///
    /// Returns the exact pins the widen rewrote with the user's authority.
    async fn whole_graph_resolve(
        &self,
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
        advance: &HashMap<AdvanceKey, TransitiveAdvance>,
        window_minutes: Option<i64>,
    ) -> Result<AuthorizedPins> {
        let inputs = Self::prepare_whole_graph_inputs(project, plan, multi_version)?;
        if !inputs.exact_pins.is_empty() {
            let minimum_age_excludes = self
                .joint_update_exclusions(project, &inputs.exact_pins)
                .await?;
            self.joint_resolve(
                project,
                &JointUpdate {
                    pins: &inputs.exact_pins,
                    filters: &inputs.importer_filters,
                    window_minutes,
                    minimum_age_excludes: &minimum_age_excludes,
                },
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
        } else if !inputs.exact_pins.is_empty() {
            // An exact direct update can land below the native age floor without taking the
            // repair leg; its plain frozen install still needs the same persisted permission.
            self.persist_minimum_age_exclusions(
                project,
                &minimum_age_repair_versions(plan),
                plan.evaluated_at,
            )
            .await?;
        }
        Ok(inputs.authorized)
    }

    fn prepare_whole_graph_inputs(
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
    ) -> Result<WholeGraphInputs> {
        let mut pins: Vec<(String, String)> = Vec::with_capacity(plan.changes.len());
        let mut importer_filters = Some(BTreeSet::new());
        let mut authorized = AuthorizedPins::default();
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
                // (Under `--rewrite` the split is not held — see `held_splits` — and takes the
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
                let rewrite = manifest::widen_constraints(
                    &project.root,
                    &change.members,
                    &change.package.name,
                    change.to.as_str(),
                    plan.rewrite,
                )?;
                authorized.record(change, plan.rewrite, plan.rewrite_pins, &rewrite);
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
            authorized,
        })
    }

    async fn joint_resolve(&self, project: &Project, update: &JointUpdate<'_>) -> Result<()> {
        let Some(args) = L::whole_graph_args(
            update.pins,
            update.filters,
            update.window_minutes,
            update.minimum_age_excludes,
        ) else {
            return Ok(());
        };
        self.cmd.run(&project.root, &args).await
    }

    /// The release-age exclusions the joint update passes so pnpm's native gate admits every pin.
    ///
    /// Each pin is exempted by its exact version: cooldown already admitted that version under the
    /// candidate's own window, and the exemption is a no-op for a pin old enough to pass anyway.
    /// The command-line list replaces the effective configured one, so its entries are carried too.
    ///
    /// # Errors
    ///
    /// See [`Self::effective_minimum_age_exclusions`].
    async fn joint_update_exclusions(
        &self,
        project: &Project,
        pins: &[(String, String)],
    ) -> Result<Vec<String>> {
        if L::NATIVE_MIN_AGE_FILE.is_none() {
            return Ok(Vec::new());
        }
        let configured = self.effective_minimum_age_exclusions(project).await?;
        Ok(crate::exclusions::grant_pins(pins, configured))
    }

    /// pnpm's effective `minimumReleaseAgeExclude` rules, in order, as pnpm reads them.
    ///
    /// # Errors
    ///
    /// A failed configuration query is a [`CoreError::System`] error, never a resolver verdict, so
    /// candidate isolation does not hold every candidate over a configuration it could not read.
    async fn effective_minimum_age_exclusions(&self, project: &Project) -> Result<Vec<String>> {
        let configured = self
            .configured_value::<ConfigStringList>(project, "minimumReleaseAgeExclude")
            .await
            .map_err(|err| {
                CoreError::System(format!(
                    "could not read pnpm's effective minimumReleaseAgeExclude: {err}"
                ))
            })?;
        Ok(crate::exclusions::configured_rules(configured))
    }

    /// Replays the plan through the override engine from the restored baseline.
    ///
    /// Returns the exact pins its own widen rewrote with the user's authority, which supersede the
    /// records of the attempt it replaces: that attempt's manifest edits were restored first.
    async fn repair_policy_rejected_graph(
        &self,
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
        advance: &HashMap<AdvanceKey, TransitiveAdvance>,
        window_minutes: Option<i64>,
    ) -> Result<AuthorizedPins> {
        // The repair replays the whole plan through the override engine, so the direct exact pins
        // and the qualified transitive pins ride one temporary config together.
        let WholeGraphInputs {
            exact_pins,
            authorized,
            ..
        } = Self::prepare_whole_graph_inputs(project, plan, multi_version)?;
        let mut pins = qualified_direct_override_pins(exact_pins);
        pins.extend(transitive_override_pins(plan, advance));
        if pins.is_empty() {
            return Ok(authorized);
        }
        self.resolve_with_temporary_overrides(project, plan, pins, window_minutes)
            .await?;
        Ok(authorized)
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
        let configured_exclusions = self.effective_minimum_age_exclusions(project).await?;
        let exact_versions = minimum_age_repair_versions(plan);
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
            .map_err(propagate_repeated_minimum_age_rejection)?;
        // This file is in the outer mutation journal, so a failed graph gate or isolated
        // candidate restores these permissions together with the settled lock.
        self.persist_minimum_age_exclusions(project, &exact_versions, plan.evaluated_at)
            .await
    }

    async fn persist_minimum_age_exclusions(
        &self,
        project: &Project,
        permitted_versions: &BTreeMap<String, BTreeSet<String>>,
        evaluated_at: Option<jiff::Timestamp>,
    ) -> Result<()> {
        let Some(native) = L::NATIVE_MIN_AGE_FILE else {
            return Ok(());
        };
        let path = project.root.join(native);
        let native_age = NativeAge::read(&path)?;
        let minutes = match native_age.minutes {
            Some(minutes) => Some(minutes),
            None => {
                self.configured_value::<Option<i64>>(project, "minimumReleaseAge")
                    .await?
            }
        };
        let locked = resolved_version_lines::<L>(&read_lock::<L>(project)?)?;
        let mut exact_versions = BTreeMap::<String, BTreeSet<String>>::new();
        if let Some(minutes) = minutes.filter(|minutes| *minutes > 0) {
            for (name, versions) in permitted_versions {
                for version in versions.iter().filter(|version| {
                    locked
                        .get(name)
                        .is_some_and(|present| present.contains(*version))
                }) {
                    let now = evaluated_at.ok_or_else(|| {
                        CoreError::System(
                            "pnpm native age admissions require the plan's evaluation clock"
                                .to_string(),
                        )
                    })?;
                    let package = PackageId::new(L::ID, name, Some(NPM.to_string()));
                    let version_id = Version::new(version);
                    let cached = self
                        .publish_times
                        .lock()
                        .await
                        .get(&package)
                        .and_then(|times| times.get(&version_id).copied());
                    let published_at = match cached {
                        Some(time) => time,
                        None => {
                            self.registry
                                .published_at(&package, &version_id, &[])
                                .await?
                        }
                    };
                    if published_at
                        .is_some_and(|time| now.duration_since(time).as_secs() / 60 < minutes)
                    {
                        exact_versions
                            .entry(name.clone())
                            .or_default()
                            .insert(version.clone());
                    }
                }
            }
        }
        match crate::exclusions::persisted_rules(native_age.exclusions, &exact_versions, &locked) {
            crate::exclusions::Persisted::Unchanged => {}
            crate::exclusions::Persisted::Rules(exclusions) => {
                set_yaml_block_list(
                    &path,
                    "minimumReleaseAgeExclude",
                    &exclusions,
                    EmptyList::IfPresent,
                    false,
                )?;
            }
            crate::exclusions::Persisted::ScalarSetting => {
                return Err(CoreError::Config(format!(
                    "{path}: minimumReleaseAgeExclude is a single string; make it a list so \
                     cooldown can record the release-age exemptions it admitted"
                )));
            }
        }
        Ok(())
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

/// Per-candidate transitive-advance verdicts against the pre-apply lock: which candidates ride the
/// qualified-override leg, and why each of the rest must be held (importer-owned name, duplicated
/// graph copies, no safe override scope).
///
/// An absent pre-apply lock has no version lines; a present but unparsable one is an error.
fn transitive_advance_verdicts<L: NodeLock>(
    plan: &Plan,
    before_content: Option<&str>,
    excluded: &HashSet<String>,
) -> Result<HashMap<AdvanceKey, TransitiveAdvance>> {
    let importer_declared = managed_importer_declarations::<L>(before_content, excluded);
    let version_lines = before_content
        .map(resolved_version_lines::<L>)
        .transpose()?
        .unwrap_or_default();
    Ok(classify_transitive_advance(
        plan,
        &importer_declared,
        &version_lines,
    ))
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

/// [`partition_catalog_managed`] against the pre-apply lock, with the run's excluded importers left
/// out of both the catalog evidence and the managed index.
fn catalog_partition<L: NodeLock>(
    plan: &Plan,
    before_content: Option<&str>,
    excluded: &HashSet<String>,
) -> (Plan, Vec<Skipped>) {
    let catalog_managed = before_content
        .map(|content| L::catalog_managed_names(content, excluded))
        .unwrap_or_default();
    let managed_index =
        before_content.map(|content| L::member_sources_excluding(content, excluded));
    partition_catalog_managed(plan, &catalog_managed, managed_index.as_ref())
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

/// The conservative hold for a name the joint resolve must not pin ([`held_splits`]): it is
/// deliberately kept in range instead of pinned, and it must not be advertised as adoptable —
/// `outdated`'s verify reclassifies it blocked.
/// A name planned at several targets is held for that alone, and its row says so: only one target
/// per name — `--major` admitting the higher line — can settle it, so the row must not blame the
/// ranges, which may every one admit each target.
/// Otherwise the detail names the [`SplitHold`] that keeps the ranges from one joint pin: two
/// resolved lines, an exact pin, a range that is not a plain caret or tilde, or a ceiling the
/// target does not cross by a major — never the ranges' mere disagreement, which by itself holds
/// nothing.
/// The reason recorded when the hold was decided (`split_holds`) wins, since only it saw the
/// manifests; the post-resolve index judges a name without one.
/// The row ends with `--rewrite`, which widens the ranges, except for a range whose floor sits
/// above the target, which no widen to that target fixes, and an unreadable manifest, which no
/// widen can rewrite.
/// An exact pin holds as [`SkipReason::PinHeld`], which unlike a range split depends on the
/// target.
/// Holding a downgrade, a manifest pin the lock does not show ends with `fix --downgrade-pinned`
/// instead, the authority that rewrites it; a pin beside another importer's range, which that
/// authority does not release, ends with editing the pin.
fn multi_version_hold(
    change: &Change,
    after_members: &MemberIndex,
    plan: &Plan,
    split_holds: &HashMap<String, SplitHold>,
) -> Skipped {
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
    let (skip_reason, reason, action) = if several_targets {
        let listed = if targets.len() > 1 {
            format!(" ({})", targets.into_iter().collect::<Vec<_>>().join(", "))
        } else {
            String::new()
        };
        (
            SkipReason::MultiVersionHeld,
            format!("planned at several targets{listed}, which one joint pin cannot land"),
            "admit one line for every importer (--major) before --rewrite can converge it"
                .to_string(),
        )
    } else {
        let hold = split_holds.get(name).cloned().or_else(|| {
            match after_members.split_verdict(name, target) {
                SplitVerdict::Held(hold) => Some(hold),
                SplitVerdict::Joint | SplitVerdict::CeilingWiden => None,
            }
        });
        let rewrite =
            format!("pass --rewrite to widen every declaring range and converge on {target}");
        let (reason, rewrite_converges) =
            split_hold_reason(hold.as_ref(), after_members, name, target);
        let lock_pin = matches!(
            hold,
            Some(SplitHold::Declaration {
                hold: version::CeilingHold::ExactPin,
                ..
            })
        );
        let manifest_pin = matches!(hold, Some(SplitHold::ManifestPin { .. }));
        // `--rewrite` is an `upgrade` flag, so a downgrade names `fix`'s own authority, and only
        // where it releases the hold: a pin beside another importer's range stays held under it.
        let action = if change.downgrade && manifest_pin {
            "rerun fix with --downgrade-pinned to rewrite the pin, or edit it".to_string()
        } else if change.downgrade && lock_pin {
            "the importers declare it under several ranges, which --downgrade-pinned does not \
             reconcile; edit the pin to move them together"
                .to_string()
        } else if rewrite_converges {
            rewrite
        } else {
            String::new()
        };
        let skip_reason = if lock_pin || manifest_pin {
            SkipReason::PinHeld
        } else {
            SkipReason::MultiVersionHeld
        };
        (skip_reason, reason, action)
    };
    Skipped {
        change: change.clone(),
        reason: skip_reason,
        offending: None,
        detail: Some(if action.is_empty() {
            format!("{lines} and {reason}; each importer is kept on its own line")
        } else {
            format!("{lines} and {reason}; each importer is kept on its own line — {action}")
        }),
    }
}

/// The held verdict for a candidate the joint resolve left short of its target: a
/// mutually-exclusive peer won, so the row names the sibling whose peer choice excluded it (the
/// candidate itself absent a unique blocker).
/// The resolve succeeded, so this is a graph ceiling, never a rejection.
/// For an attempted transitive advance (`advanced`) the unattributed message hides the real
/// cause — whether the qualified override never matched (an exact or sub-line dependent range)
/// or the override-free settlement reverted the pin, the target sits outside some dependent's
/// declared range — so the detail says that instead.
fn graph_ceiling_hold<L: NodeLock>(
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
        reason: SkipReason::GraphCeilingHeld,
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
/// major-line qualifier (`semver` pinned to `7.7.3` becomes `semver@^7.0.0`).
/// An unqualified `name: target` override captures *every* same-name request in the graph, so an
/// undeclared copy on another major would be silently collapsed onto the direct target — and the
/// newest-copy report projection would never show it.
/// Qualified to the target's line, the override addresses the requests on the line the plan
/// steers (after widening, the declaring importer's range lives on that line) while a foreign line
/// keeps its own resolution.
/// The qualifier scopes by range, not by importer: pnpm overrides a request whose range intersects
/// the key in an importer the run excludes too, a move the settled-lock guard reports
/// ([`report_excluded_moves`]) or, for an exact manifest pin, refuses ([`ProtectedPins`]).
/// A target with no parsable major keeps the unqualified key: better a broad pin than a repair that
/// cannot steer its own candidate.
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

/// The exclusion list the repair and settlement installs pass: the configured rules, in order,
/// granting every version the repair lands ([`minimum_age_repair_versions`]).
///
/// A targeted package's rejected starting pin is withheld from its exact first-match rule: allowing
/// it would let the settlement resolve float back to it after the temporary override is removed.
/// A package whose first-match rule exempts every version keeps that exemption, since pnpm cannot
/// express every version but one ([`crate::exclusions::set_exact_permission`]).
fn minimum_age_repair_exclusions(plan: &Plan, configured_exclusions: Vec<String>) -> Vec<String> {
    let targeted = plan
        .changes
        .iter()
        .map(|change| change.package.name.as_str())
        .collect::<HashSet<_>>();
    let mut exclusions = configured_exclusions;
    for (name, granted) in minimum_age_repair_versions(plan) {
        let withheld: BTreeSet<String> = if targeted.contains(name.as_str()) {
            plan.baseline_violations
                .iter()
                .filter(|violation| violation.package.name == name)
                .filter_map(|violation| {
                    crate::exclusions::version_identity(violation.version.as_str())
                })
                .collect()
        } else {
            BTreeSet::new()
        };
        crate::exclusions::set_exact_permission(&mut exclusions, &name, &withheld, &granted);
    }
    exclusions
}

fn minimum_age_repair_versions(plan: &Plan) -> BTreeMap<String, BTreeSet<String>> {
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
    exact_versions
}

/// The post-resolve lock inconsistency, but only when this resolve introduced it.
///
/// A mismatch already present in the pre-apply lock (e.g. a pnpm `overrides` entry that legally pins
/// a direct dependency outside its declared range — `--frozen-lockfile` accepts that via the lock's
/// `overrides:` section, which the cheap check does not read) must not fail apply: every recovery
/// trial would fail identically and the run would misreport all candidates as held.
/// The before/after gate also absorbs any divergence of the check from node-semver: whatever the
/// check misjudges, it misjudges identically on both sides.
fn new_lock_inconsistency<L: NodeLock>(before: Option<&str>, after: &str) -> Option<String> {
    let detail = L::lock_consistency_error(after)?;
    before
        .is_none_or(|content| L::lock_consistency_error(content).is_none())
        .then_some(detail)
}

/// Why a range-held name stays off the joint pin, phrased for its hold row, and whether
/// `--rewrite` converges it.
fn split_hold_reason(
    hold: Option<&SplitHold>,
    after_members: &MemberIndex,
    name: &str,
    target: &str,
) -> (String, bool) {
    use crate::version::CeilingHold;
    let reason = match hold {
        Some(SplitHold::SeveralLines { ranges, .. }) => {
            let proven = ranges
                .iter()
                .all(|range| version::range_match(range, target) == version::RangeMatch::Excludes);
            let listed = ranges.join(", ");
            if proven {
                let verb = if ranges.len() == 1 {
                    "excludes"
                } else {
                    "exclude"
                };
                format!("{listed} {verb} {target}")
            } else {
                format!("not every declared range is proven to admit {target} ({listed})")
            }
        }
        Some(SplitHold::Unresolved) => {
            format!("the lock resolves none of its declarations, so {target} cannot be judged")
        }
        Some(SplitHold::NoRange) => format!("no plain range to judge against {target}"),
        Some(SplitHold::ManifestUnavailable { manifest }) => {
            return (
                format!(
                    "{manifest} could not be read to confirm that every declaration may widen to \
                     {target}; restore or repair it"
                ),
                false,
            );
        }
        Some(SplitHold::ManifestPin { range }) => {
            format!("the exact pin {range} excludes {target}")
        }
        Some(SplitHold::Declaration { range, hold }) => match hold {
            CeilingHold::ExactPin => format!("the exact pin {range} excludes {target}"),
            CeilingHold::NotPlain => {
                format!("{range} is not a plain caret or tilde range --major may widen to {target}")
            }
            CeilingHold::WithinLine => format!(
                "{range} excludes {target} within its own major line, which --major does not widen"
            ),
            CeilingHold::BelowFloor => {
                return (format!("{range} sets a floor above {target}"), false);
            }
        },
        None => format!(
            "not every declared range admits {target} ({})",
            after_members.declared_specifiers_of(name).join(", ")
        ),
    };
    (reason, true)
}

/// The planned names the whole-graph resolve must not exact-pin, with why each is held.
pub(crate) struct HeldSplits {
    /// Every held name.
    pub(crate) names: HashSet<String>,
    /// Why each name held by its declared ranges is held, where the reason is known.
    /// A name held for several planned targets has no entry.
    pub(crate) reasons: HashMap<String, SplitHold>,
}

/// The planned names the whole-graph resolve must NOT exact-pin: those whose workspace declarations
/// split under the planned target ([`MemberIndex::split_verdict`]) — exact-pinning would drag some
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
/// A name whose differently spelled ranges only need the cross-major ceiling widen
/// ([`SplitVerdict::CeilingWiden`]) is pinned like a lone declaration once every install-field
/// range in its declaring members' manifests under `root` agrees.
/// The lock records one specifier per importer, so a second install field (an exact pin under
/// `devDependencies` beside a caret under `dependencies`) is visible only there, and the widen
/// would rewrite it too.
/// Without a `root`, or with a declaring manifest that cannot be read, the name stays held.
/// Only a declared (direct) change is confirmed this way: a transitive copy of the same name has
/// no declaring manifest and is held by its own transitive verdict.
///
/// A name one joint pin lands outright ([`SplitVerdict::Joint`]) is held all the same when one of
/// its declaring manifests pins it exactly, in any install field, to a version excluding the target
/// ([`manifest::excluding_exact_pin`]), since the Auto widen would rewrite that pin.
/// Only the plan's authority to rewrite pins releases it, for a downgrade ([`holds_exact_pins`]).
/// Without a `root` no manifest can show such a pin, so the name is pinned.
/// A declaring manifest that cannot be read holds it.
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
pub(crate) fn held_splits<L: NodeLock>(
    root: Option<&Utf8Path>,
    content: Option<&str>,
    plan: &Plan,
) -> HeldSplits {
    let index = content.map(|content| L::member_sources_excluding(content, &excluded_paths(plan)));
    let targets = planned_targets(plan);
    let mut splits = HeldSplits {
        names: HashSet::new(),
        reasons: HashMap::new(),
    };
    for change in &plan.changes {
        let name = change.package.name.as_str();
        let target = change.to.as_str();
        if targets.get(name).is_some_and(|targets| targets.len() > 1) {
            splits.names.insert(name.to_string());
            continue;
        }
        let Some(index) = index.as_ref().filter(|_| plan.rewrite == RewriteMode::Auto) else {
            continue;
        };
        let hold = match index.split_verdict(name, target) {
            // The lock records one specifier per importer, so an exact pin in a second install
            // field is visible only in the manifest, where the widen would rewrite it.
            SplitVerdict::Joint if !holds_exact_pins(change, plan.rewrite, plan.rewrite_pins) => {
                continue;
            }
            SplitVerdict::Joint => match root.and_then(|root| manifest_pin_hold(root, change)) {
                None => continue,
                hold => hold,
            },
            SplitVerdict::Held(hold) => Some(hold),
            // A copy no importer declares has no manifest to confirm, and its own transitive
            // verdict holds it; only the declared copy decides whether the name is held.
            SplitVerdict::CeilingWiden if !change.direct => continue,
            SplitVerdict::CeilingWiden => match root {
                Some(root) => match manifest_ceiling_hold(root, change) {
                    None => continue,
                    hold => hold,
                },
                None => None,
            },
        };
        splits.names.insert(name.to_string());
        if let Some(hold) = hold {
            splits.reasons.insert(name.to_string(), hold);
        }
    }
    splits
}

/// The first install-field range in `change`'s declaring manifests that the cross-major ceiling
/// widen cannot carry to its target, or the manifest that could not be read to tell.
fn manifest_ceiling_hold(root: &Utf8Path, change: &Change) -> Option<SplitHold> {
    let mut ranges = Vec::new();
    for rel in manifest::manifest_rels(&change.members) {
        match manifest::install_ranges(&root.join(&rel), &change.package.name) {
            Ok(found) => ranges.extend(found),
            Err(_) => {
                return Some(SplitHold::ManifestUnavailable {
                    manifest: manifest::display_rel(&rel),
                });
            }
        }
    }
    SplitHold::first_declaration_hold(ranges.iter().map(String::as_str), change.to.as_str())
}

/// The exact install-field pin in `change`'s declaring manifests that excludes its target, or the
/// manifest that could not be read to tell; a missing manifest pins nothing.
fn manifest_pin_hold(root: &Utf8Path, change: &Change) -> Option<SplitHold> {
    match manifest::excluding_exact_pin(
        root,
        &change.members,
        &change.package.name,
        change.to.as_str(),
    ) {
        Ok(pin) => pin.map(|manifest::ExcludingPin { range, .. }| SplitHold::ManifestPin { range }),
        Err(manifest::UnreadableManifest { manifest, .. }) => {
            Some(SplitHold::ManifestUnavailable { manifest })
        }
    }
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

    fn guards_duplicate_copies(&self) -> bool {
        L::supports_whole_graph_resolve()
    }

    fn supports_transitive_advance(&self) -> bool {
        L::SUPPORTS_TRANSITIVE_ADVANCE
    }

    fn target_fallback(&self) -> TargetFallback {
        // A cross-major target is held for reasons the current line escapes (a dependent's peer
        // range, a range split across importers), so the in-line move `--no-major` would take is
        // the alternative worth one more trial.
        TargetFallback::CurrentLine
    }

    fn external_resolve_roots(&self, project: &Project) -> Vec<Utf8PathBuf> {
        // Declared patch files are in-tree, but their `.patch` basename matches no resolver input.
        crate::patches::declared_patch_files(&project.root)
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        journal::<L>(project, plan)
    }

    async fn mutation_baseline(&self, project: &Project, plan: &Plan) -> Result<MutationBaseline> {
        baseline::<L>(project, plan)
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        self.apply_with_runner(mutation, &self.cmd).await
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

    fn requires_sync_admission_evaluation(&self, _project: &Project) -> bool {
        L::NATIVE_MIN_AGE_FILE.is_some()
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
        // Selectors cooldown could not prove exempt and selectors pnpm would read differently stay
        // out of the native list and are reported on every outcome, including a sync that cannot
        // write the window.
        let selectors = NativeSelectors::partition(&policy.exempt_packages);
        let mut warnings = withheld_selector_warnings(&policy.withheld_exemptions);
        warnings.extend(omitted_selector_warnings(&selectors.omitted));
        let Some(minutes) = policy.default_window.as_ref().and_then(window_minutes) else {
            // pnpm's minimumReleaseAge is a rolling minute count; a freeze date or opt-out can't be
            // expressed, so the window is left as it is.
            // The window may still be active, so a selector the policy no longer grants loses its
            // entry.
            // An existing key always survives as at least `[]`; only a withheld or omitted
            // selector calls for adding one, since the written selectors are not added here.
            let empty = if policy.withheld_exemptions.is_empty() && selectors.omitted.is_empty() {
                EmptyList::IfPresent
            } else {
                EmptyList::Explicit
            };
            let changed = prune_exclusions(&path, &selectors.written, empty, dry_run)?;
            return Ok(if changed {
                SyncReport::Written { path, warnings }
            } else {
                SyncReport::Unchanged { path, warnings }
            });
        };
        // Policy-exempt names and admitted young versions must also pass pnpm's rolling age gate.
        let exclusions = if let Some(admitted) = &policy.admitted_versions {
            let mut exact_versions = BTreeMap::<String, BTreeSet<String>>::new();
            for admitted in admitted {
                exact_versions
                    .entry(admitted.name.clone())
                    .or_default()
                    .insert(admitted.version.to_string());
            }
            minimum_age_exclusions(&selectors.written, &exact_versions)
        } else {
            // Without complete evidence, preserve exact entry strings even for missing pins.
            // Policy globs and the default window can still be synchronized safely.
            let existing = NativeAge::read(&path)?.exclusions.into_vec();
            let mut exclusions = minimum_age_exclusions(&selectors.written, &BTreeMap::new());
            exclusions.extend(
                existing
                    .into_iter()
                    .filter(|entry| exact_exclusion(entry).is_some()),
            );
            exclusions
        };
        // An emptied list stays as an explicit `[]` wherever the key exists, since removing it
        // would expose the exclusions pnpm inherits from user or global configuration.
        // A policy without selectors leaves an absent key absent, while a policy whose selectors
        // were all withheld or omitted adds the explicit empty list, which masks the inherited
        // exclusions just as a non-empty list does.
        let empty = if policy.exempt_packages.is_empty() && policy.withheld_exemptions.is_empty() {
            EmptyList::IfPresent
        } else {
            EmptyList::Explicit
        };
        let mut changed =
            set_yaml_scalar(&path, "minimumReleaseAge", &minutes.to_string(), dry_run)?;
        changed |= set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &exclusions,
            empty,
            dry_run,
        )?;
        Ok(if changed {
            SyncReport::Written { path, warnings }
        } else {
            SyncReport::Unchanged { path, warnings }
        })
    }
}

/// One [`DiagnosticKind::StricterNative`] warning per selector cooldown withholds from pnpm's
/// `minimumReleaseAgeExclude` because it cannot prove the selector exempts every release.
fn withheld_selector_warnings(withheld: &[String]) -> Vec<Diagnostic> {
    withheld
        .iter()
        .map(|selector| {
            Diagnostic::new(
                DiagnosticKind::StricterNative,
                format!(
                    "cooldown cannot prove that every release `{selector}` matches is exempt, so \
                     it is not exempted natively"
                ),
            )
        })
        .collect()
}

/// One [`DiagnosticKind::StricterNative`] warning per selector `sync` leaves out of pnpm's
/// `minimumReleaseAgeExclude`, since pnpm's age gate then still governs the packages it exempts.
fn omitted_selector_warnings(omitted: &[String]) -> Vec<Diagnostic> {
    omitted
        .iter()
        .map(|selector| {
            Diagnostic::new(
                DiagnosticKind::StricterNative,
                format!(
                    "selector `{selector}` uses syntax sync does not write to pnpm's \
                     minimumReleaseAgeExclude, so pnpm's minimumReleaseAge applies to its packages \
                     unless another written rule exempts them"
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod berry_attribution_tests;

#[cfg(test)]
mod npm_attribution_tests;

#[cfg(all(test, unix))]
mod pnpm_verify_tests;

#[cfg(test)]
mod yarn_attribution_tests;

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
        let pin_exclusions = [
            "@typescript-eslint/eslint-plugin@8.0.0".to_string(),
            "eslint@9.5.0".to_string(),
        ];
        assert_eq!(
            Pnpm::whole_graph_args(&pins, &filters, Some(20160), &pin_exclusions),
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
                "--config.minimumReleaseAgeExclude=@typescript-eslint/eslint-plugin@8.0.0"
                    .to_string(),
                "--config.minimumReleaseAgeExclude=eslint@9.5.0".to_string(),
            ])
        );
        assert_eq!(
            Pnpm::whole_graph_args(&pins, &[], None, &[]),
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
        assert_eq!(Pnpm::whole_graph_args(&[], &filters, None, &[]), None);
        // npm/yarn/bun have no joint resolve, so they keep the per-package path.
        assert!(!Npm::supports_whole_graph_resolve());
        assert!(Pnpm::supports_whole_graph_resolve());
        assert_eq!(
            Npm::whole_graph_args(&pins, &filters, Some(20160), &[]),
            None
        );
        assert_eq!(
            crate::lock::Yarn::whole_graph_args(&pins, &filters, None, &[]),
            None
        );
        assert_eq!(
            crate::lock::Bun::whole_graph_args(&[], &filters, None, &[]),
            None
        );
    }

    /// pnpm reads a lone repeated-list `--config` flag as a string, one pattern per character, so a
    /// single exclusion must still reach pnpm as a list on both the joint update and the policy
    /// repair.
    #[test]
    fn a_single_minimum_age_exclusion_is_passed_as_a_list() {
        let single = ["lodash@4.17.21".to_string()];
        let doubled = [
            "--config.minimumReleaseAgeExclude=lodash@4.17.21".to_string(),
            "--config.minimumReleaseAgeExclude=lodash@4.17.21".to_string(),
        ];
        let update = Pnpm::whole_graph_args(
            &[("lodash".to_string(), "4.17.21".to_string())],
            &[".".to_string()],
            Some(20160),
            &single,
        )
        .unwrap();
        assert!(update.ends_with(&doubled), "{update:?}");
        for resolution_only in [true, false] {
            let repair = Pnpm::policy_repair_args(Some(20160), &single, resolution_only).unwrap();
            assert!(repair.ends_with(&doubled), "{repair:?}");
        }
    }

    /// The repair keeps the configured rules in order, grants the repair's destinations in each
    /// package's first matching rule, and withholds a targeted package's rejected starting pin.
    #[test]
    fn minimum_age_repair_exclusions_keep_the_configured_order() {
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
                vec![
                    "@typescript-eslint/*".to_string(),
                    "nanoid".to_string(),
                    "eslint@10.5.0".to_string(),
                    "eslint@10.7.0".to_string(),
                    "flatted@3.4.1".to_string()
                ],
            ),
            vec![
                "@typescript-eslint/*".to_string(),
                "nanoid".to_string(),
                "eslint@10.5.0||10.6.0".to_string(),
                // Shadowed by the first `eslint` rule, so its rejected pin stays unexempt.
                "eslint@10.7.0".to_string(),
                "flatted@3.4.1||3.4.2||3.4.3".to_string(),
            ]
        );
        let exclusions = minimum_age_repair_exclusions(
            &plan,
            vec!["eslint@10.5.0".to_string(), "eslint@10.7.0".to_string()],
        );
        assert!(crate::exclusions::exempts(&exclusions, "eslint", "10.6.0"));
        assert!(!crate::exclusions::exempts(&exclusions, "eslint", "10.7.0"));
        // An untargeted baseline violation is granted so the starting lock passes pnpm's gate.
        assert!(crate::exclusions::exempts(&exclusions, "flatted", "3.4.2"));
    }

    /// A rejected starting pin listed in the target's own first rule is withheld in place, so the
    /// wildcard behind that rule never takes over the package.
    #[test]
    fn minimum_age_repair_exclusions_withhold_without_exposing_a_later_rule() {
        let plan = Plan {
            changes: vec![change("foo", "1.0.0", "2.0.0")],
            baseline_violations: vec![cooldown_core::BaselineViolation {
                package: PackageId::new(ToolId(NPM), "foo", None),
                version: Version::new("1.0.0"),
            }],
            ..Plan::default()
        };
        let exclusions =
            minimum_age_repair_exclusions(&plan, vec!["foo@1.0.0".to_string(), "*".to_string()]);
        assert_eq!(exclusions, vec!["foo@2.0.0".to_string(), "*".to_string()]);
        assert!(!crate::exclusions::exempts(&exclusions, "foo", "1.0.0"));
        assert!(crate::exclusions::exempts(&exclusions, "bar", "9.0.0"));
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

    /// An npm alias's installed copy is locked under its target's name, never the alias's.
    #[test]
    fn locked_versions_name_an_npm_alias_by_its_target() -> eyre::Result<()> {
        let lock = indoc! {r#"
            {
              "lockfileVersion": 3,
              "packages": {
                "": { "name": "root", "dependencies": { "foo": "npm:other@2.0.0", "other": "^1.0.0" } },
                "node_modules/foo": { "name": "other", "version": "2.0.0" },
                "node_modules/other": { "version": "1.0.0" }
              }
            }
        "#};
        let versions = locked_versions::<Npm>(lock)?;
        assert_eq!(versions.get("other").map(String::as_str), Some("2.0.0"));
        assert_eq!(versions.get("foo"), None);
        Ok(())
    }

    /// A version only an alias installs is the target's identity but no copy of the member's own
    /// declaration of that name: it is transitive, owned by no member, and never pinned, so no
    /// move of it can rewrite the ordinary range, while the ordinary copy stays direct and owned.
    #[tokio::test]
    async fn an_alias_only_version_gets_no_editable_members() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        for (other, pinned) in [("^1.0.0", false), ("1.0.0", true)] {
            std::fs::write(
                root.join("package.json"),
                indoc::formatdoc! {r#"
                    {{ "name": "root", "dependencies": {{ "foo": "npm:other@2.0.0", "other": "{other}" }} }}
                "#},
            )?;
            std::fs::write(
                root.join("package-lock.json"),
                indoc::formatdoc! {r#"
                    {{
                      "lockfileVersion": 3,
                      "packages": {{
                        "": {{ "name": "root", "dependencies": {{ "foo": "npm:other@2.0.0", "other": "{other}" }} }},
                        "node_modules/foo": {{ "name": "other", "version": "2.0.0" }},
                        "node_modules/other": {{ "version": "1.0.0" }}
                      }}
                    }}
                "#},
            )?;
            let project = Project {
                root: root.clone(),
                kind: Npm::ID,
                manifest: root.join("package.json"),
                exclude_newer: None,
                exclude_newer_package: std::collections::BTreeMap::new(),
                generated_members: cooldown_core::GeneratedMembers::undeclared(),
            };

            let deps = tool().dependencies(&project, DepScope::Graph).await?;

            let rows: Vec<(String, String, bool, Vec<String>, bool)> = deps
                .iter()
                .map(|dep| {
                    (
                        dep.package.name.clone(),
                        dep.current.as_str().to_string(),
                        dep.direct,
                        dep.members
                            .iter()
                            .map(|member| member.path.clone())
                            .collect(),
                        dep.pinned,
                    )
                })
                .collect();
            let mut rows = rows;
            rows.sort();
            assert_eq!(
                rows,
                [
                    (
                        "other".to_string(),
                        "1.0.0".to_string(),
                        true,
                        vec![".".to_string()],
                        pinned
                    ),
                    (
                        "other".to_string(),
                        "2.0.0".to_string(),
                        false,
                        Vec::new(),
                        false
                    ),
                ],
                "{other}"
            );
            let direct = tool().dependencies(&project, DepScope::Direct).await?;
            assert_eq!(
                direct
                    .iter()
                    .map(|dep| format!("{}@{}", dep.package.name, dep.current.as_str()))
                    .collect::<Vec<_>>(),
                ["other@1.0.0"],
                "{other}"
            );
        }
        Ok(())
    }

    /// A local workspace package of the alias target's name and version is another package than
    /// the registry release the alias names, so it gives the alias's version no claim on the
    /// ordinary declaration: the version stays transitive and unowned, the manifest untouched.
    #[tokio::test]
    async fn a_local_source_gives_an_alias_only_version_no_editable_members() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let manifest = indoc! {r#"
            {
              "name": "root",
              "dependencies": {
                "other": "^1.0.0",
                "foo": "npm:other@2.0.0",
                "local": "file:./packages/other"
              }
            }
        "#};
        std::fs::write(root.join("package.json"), manifest)?;
        std::fs::write(
            root.join("package-lock.json"),
            indoc! {r#"
                {
                  "lockfileVersion": 3,
                  "packages": {
                    "": {
                      "name": "root",
                      "dependencies": {
                        "other": "^1.0.0",
                        "foo": "npm:other@2.0.0",
                        "local": "file:./packages/other"
                      }
                    },
                    "node_modules/other": { "version": "1.0.0" },
                    "node_modules/foo": { "name": "other", "version": "2.0.0" },
                    "node_modules/local": { "resolved": "packages/other", "link": true },
                    "packages/other": { "name": "other", "version": "2.0.0" }
                  }
                }
            "#},
        )?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };

        let deps = tool().dependencies(&project, DepScope::Graph).await?;

        let mut rows: Vec<(String, String, bool, usize)> = deps
            .iter()
            .map(|dep| {
                (
                    dep.package.name.clone(),
                    dep.current.as_str().to_string(),
                    dep.direct,
                    dep.members.len(),
                )
            })
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            [
                ("other".to_string(), "1.0.0".to_string(), true, 1),
                ("other".to_string(), "2.0.0".to_string(), false, 0),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("package.json"))?,
            manifest
        );
        Ok(())
    }

    /// One dependency row as the alias-ownership tests compare it.
    #[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct OwnershipRow {
        name: String,
        version: String,
        direct: bool,
        declared_bound: Option<String>,
        members: Vec<String>,
        pinned: bool,
    }

    /// The rows of a lone root project's graph and the `name@version` of its direct scope.
    struct Ownership {
        graph: Vec<OwnershipRow>,
        direct: Vec<String>,
    }

    /// Reads [`Ownership`] from a root project with `manifest` and `package-lock.json` `lock`.
    async fn npm_ownership(manifest: &str, lock: &str) -> eyre::Result<Ownership> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("package.json"), manifest)?;
        std::fs::write(root.join("package-lock.json"), lock)?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let mut graph: Vec<OwnershipRow> = tool()
            .dependencies(&project, DepScope::Graph)
            .await?
            .into_iter()
            .map(|dep| OwnershipRow {
                name: dep.package.name,
                version: dep.current.as_str().to_string(),
                direct: dep.direct,
                declared_bound: dep.declared_bound,
                members: dep.members.into_iter().map(|member| member.path).collect(),
                pinned: dep.pinned,
            })
            .collect();
        graph.sort();
        let mut direct: Vec<String> = tool()
            .dependencies(&project, DepScope::Direct)
            .await?
            .iter()
            .map(|dep| format!("{}@{}", dep.package.name, dep.current.as_str()))
            .collect();
        direct.sort();
        Ok(Ownership { graph, direct })
    }

    /// One root dependency of a v1 fixture: the `version` its lock entry records and the
    /// specifier its manifest declares.
    struct V1Entry<'a> {
        name: &'a str,
        locked: &'a str,
        declared: &'a str,
    }

    /// The manifest and `package-lock.json` (v1) of a root project.
    struct V1Project {
        manifest: String,
        lock: String,
    }

    /// A root project declaring each of `entries` and locking it in a v1 `dependencies` tree.
    fn npm_v1_project(entries: &[V1Entry<'_>]) -> V1Project {
        let manifest_entries: Vec<String> = entries
            .iter()
            .map(|V1Entry { name, declared, .. }| format!(r#""{name}": "{declared}""#))
            .collect();
        let lock_entries: Vec<String> = entries
            .iter()
            .map(|V1Entry { name, locked, .. }| format!(r#""{name}": {{ "version": "{locked}" }}"#))
            .collect();
        V1Project {
            manifest: indoc::formatdoc! {r#"
                {{ "name": "root", "dependencies": {{ {} }} }}
            "#, manifest_entries.join(", ")},
            lock: indoc::formatdoc! {r#"
                {{ "name": "root", "lockfileVersion": 1, "dependencies": {{ {} }} }}
            "#, lock_entries.join(", ")},
        }
    }

    /// A v1 fixture entry whose manifest declares the same `npm:` or protocol reference its lock
    /// records.
    fn v1_verbatim<'a>(name: &'a str, reference: &'a str) -> V1Entry<'a> {
        V1Entry {
            name,
            locked: reference,
            declared: reference,
        }
    }

    /// A v1 alias's target is no copy of the root's own declaration of that name.
    /// v1 attributes directness by name from the root manifest, so the target must be held out of
    /// it explicitly: it stays transitive with no bound, while the genuine copy keeps the
    /// declaration and its bound.
    /// v1 records no exact-pin evidence, so neither row is pinned even under an exact range.
    #[tokio::test]
    async fn a_v1_alias_only_version_gets_no_editable_members() -> eyre::Result<()> {
        // An exact pin has no explicit upper bound, so only the range is a declared bound.
        for (other, bound) in [(">=1.0.0 <1.5.0", Some(">=1.0.0 <1.5.0")), ("1.0.0", None)] {
            let V1Project { manifest, lock } = npm_v1_project(&[
                v1_verbatim("foo", "npm:other@2.0.0"),
                V1Entry {
                    name: "other",
                    locked: "1.0.0",
                    declared: other,
                },
            ]);

            let Ownership { graph, direct } = npm_ownership(&manifest, &lock).await?;

            assert_eq!(
                graph,
                [
                    OwnershipRow {
                        name: "other".to_string(),
                        version: "1.0.0".to_string(),
                        direct: true,
                        declared_bound: bound.map(str::to_string),
                        members: Vec::new(),
                        pinned: false,
                    },
                    OwnershipRow {
                        name: "other".to_string(),
                        version: "2.0.0".to_string(),
                        direct: false,
                        declared_bound: None,
                        members: Vec::new(),
                        pinned: false,
                    },
                ],
                "{other}"
            );
            assert_eq!(direct, ["other@1.0.0"], "{other}");
        }
        Ok(())
    }

    /// A genuine registry copy of the alias target's release gives that release back its ordinary
    /// claim on the root's declaration, whichever side of the alias the lock lists it on.
    /// A local source of the name gives no such claim, and an alias of its own name is no foreign
    /// alias at all.
    #[tokio::test]
    async fn a_v1_registry_copy_restores_an_alias_targets_classification() -> eyre::Result<()> {
        // Alias keys sort before and after `other`, so both entry orders are covered.
        for alias in ["foo", "zzz"] {
            let V1Project { manifest, lock } = npm_v1_project(&[
                v1_verbatim(alias, "npm:other@2.0.0"),
                V1Entry {
                    name: "other",
                    locked: "2.0.0",
                    declared: ">=2.0.0 <3.0.0",
                },
            ]);

            let Ownership { graph, direct } = npm_ownership(&manifest, &lock).await?;

            assert_eq!(
                graph,
                [OwnershipRow {
                    name: "other".to_string(),
                    version: "2.0.0".to_string(),
                    direct: true,
                    declared_bound: Some(">=2.0.0 <3.0.0".to_string()),
                    members: Vec::new(),
                    pinned: false,
                }],
                "{alias}"
            );
            assert_eq!(direct, ["other@2.0.0"], "{alias}");
        }

        // A local source yields no row of its own and leaves the alias target unowned.
        let V1Project { manifest, lock } = npm_v1_project(&[
            v1_verbatim("foo", "npm:other@2.0.0"),
            v1_verbatim("other", "file:packages/other"),
        ]);
        let Ownership { graph, direct } = npm_ownership(&manifest, &lock).await?;
        assert_eq!(
            graph,
            [OwnershipRow {
                name: "other".to_string(),
                version: "2.0.0".to_string(),
                direct: false,
                declared_bound: None,
                members: Vec::new(),
                pinned: false,
            }]
        );
        assert_eq!(direct, [] as [String; 0]);

        // An alias of its own name is the ordinary direct dependency.
        let V1Project { manifest, lock } = npm_v1_project(&[v1_verbatim("foo", "npm:foo@2.0.0")]);
        let Ownership { graph, direct } = npm_ownership(&manifest, &lock).await?;
        assert_eq!(
            graph,
            [OwnershipRow {
                name: "foo".to_string(),
                version: "2.0.0".to_string(),
                direct: true,
                declared_bound: None,
                members: Vec::new(),
                pinned: false,
            }]
        );
        assert_eq!(direct, ["foo@2.0.0"]);
        Ok(())
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

    /// An exact pin is judged as npm reads it, an equality, under the same before/after gate.
    #[test]
    fn a_floated_exact_pin_is_charged_only_to_the_resolve_that_floated_it() {
        let lock = |specifier: &str, version: &str| {
            indoc::formatdoc! {"
                lockfileVersion: '9.0'

                importers:

                  apps/admin:
                    dependencies:
                      vite:
                        specifier: {specifier}
                        version: {version}(@types/node@22.19.20)
            "}
        };
        for specifier in ["6.0.0", "v6.0.0", "= v6.0.0"] {
            let clean = lock(specifier, "6.0.0");
            let stale = lock(specifier, "6.1.0");

            // The resolve floated the pin within its major (clean or absent before), so it surfaces.
            assert!(
                new_lock_inconsistency::<Pnpm>(Some(&clean), &stale).is_some(),
                "{specifier}"
            );
            assert!(
                new_lock_inconsistency::<Pnpm>(None, &stale).is_some(),
                "{specifier}"
            );
            // A floated pin the lock already carried is not this resolve's doing.
            assert_eq!(
                new_lock_inconsistency::<Pnpm>(Some(&stale), &stale),
                None,
                "{specifier}"
            );
            // A resolve that brings the pin back to its version settles cleanly.
            assert_eq!(
                new_lock_inconsistency::<Pnpm>(Some(&stale), &clean),
                None,
                "{specifier}"
            );
        }
    }

    /// A resolve that moves a stable version to a prerelease the specifier opts in is consistent.
    /// `1.x >=1.2.3-beta.1` admits `1.2.3-beta.2` in node-semver, since the explicit comparator
    /// opts that tuple in and `1.x` expands to bounds around it.
    #[test]
    fn a_prerelease_its_comparator_set_opts_in_is_not_charged_to_the_resolve() {
        let lock = |version: &str| {
            indoc::formatdoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      dep:
                        specifier: '1.x >=1.2.3-beta.1'
                        version: {version}
            "}
        };
        let stable = lock("1.9.0");
        let prerelease = lock("1.2.3-beta.2");

        assert_eq!(Pnpm::lock_consistency_error(&prerelease), None);
        assert_eq!(
            new_lock_inconsistency::<Pnpm>(Some(&stable), &prerelease),
            None
        );
    }

    /// A resolve that moves a stable version to a `0.0.0` prerelease its own comparator opts in is
    /// consistent.
    /// node-semver drops the `>=0.0.0` floor of `^0.0`, so `^0.0 >=0.0.0-alpha` admits
    /// `0.0.0-beta`.
    #[test]
    fn a_zero_prerelease_beside_a_dropped_zero_floor_is_not_charged_to_the_resolve() {
        let lock = |version: &str| {
            indoc::formatdoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      dep:
                        specifier: '^0.0 >=0.0.0-alpha'
                        version: {version}
            "}
        };
        let stable = lock("0.0.4");
        let prerelease = lock("0.0.0-beta");

        assert_eq!(Pnpm::lock_consistency_error(&stable), None);
        assert_eq!(Pnpm::lock_consistency_error(&prerelease), None);
        assert_eq!(
            new_lock_inconsistency::<Pnpm>(Some(&stable), &prerelease),
            None
        );
    }

    /// A persisted pnpm override legally resolves a direct exact pin off its pin.
    /// The lock records the manifest's specifier beside the overridden version, so the check flags
    /// it, and only the before/after gate keeps that legal lock from failing every apply.
    #[test]
    fn a_persisted_override_of_an_exact_pin_stays_grandfathered() {
        let overridden = |sibling: &str| {
            indoc::formatdoc! {"
                lockfileVersion: '9.0'

                overrides:
                  vite: 6.1.0

                importers:

                  .:
                    dependencies:
                      chalk:
                        specifier: ^4.1.0
                        version: {sibling}
                      vite:
                        specifier: 6.0.0
                        version: 6.1.0
            "}
        };
        let before = overridden("4.1.0");
        let after = overridden("4.1.2");

        // The check cannot read the override, so the legal lock alone looks inconsistent.
        assert!(Pnpm::lock_consistency_error(&before).is_some());
        // A sibling update that keeps the override is not charged with its mismatch.
        assert_eq!(new_lock_inconsistency::<Pnpm>(Some(&before), &after), None);
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
        assert_eq!(
            collateral_changes::<Pnpm>(&before, &after, &stale_duplicate_baseline),
            []
        );
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let policy = cooldown_core::ResolvedPolicy {
            default_window: Some(cooldown_core::WindowSpec::MinAge(
                jiff::SignedDuration::from_hours(24 * 14),
            )),
            exempt_packages: vec!["@typescript/native-preview".to_string()],
            withheld_exemptions: Vec::new(),
            package_windows: std::collections::BTreeMap::new(),
            admitted_versions: Some(Vec::new()),
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

    #[tokio::test]
    async fn write_native_preserves_exact_entries_verbatim_when_evaluation_fails()
    -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let before = indoc! {r#"
            packages: []
            minimumReleaseAge: 120
            minimumReleaseAgeExclude:
              - "old-glob"
              - "@corp/pkg@2.0.1||1.2.2"
              - "gone@1.0.0"
              - "gone@1.0.0"
        "#};
        std::fs::write(&path, before)?;
        let project = Project {
            root: root.clone(),
            kind: Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let policy = ResolvedPolicy {
            default_window: Some(cooldown_core::WindowSpec::MinAge(
                jiff::SignedDuration::from_hours(24 * 14),
            )),
            exempt_packages: vec!["new-glob".to_string(), "@corp/*".to_string()],
            withheld_exemptions: Vec::new(),
            package_windows: std::collections::BTreeMap::new(),
            admitted_versions: None,
        };
        let cache = tempfile::tempdir()?;
        let tool = NpmTool::<Pnpm>::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        std::assert_matches!(
            tool.write_native(&project, &policy, true).await?,
            SyncReport::Written { .. }
        );
        assert_eq!(std::fs::read_to_string(&path)?, before);
        tool.write_native(&project, &policy, false).await?;
        let written = NativeAge::read(&path)?;
        assert_eq!(written.minutes, Some(20160));
        assert_eq!(
            written.exclusions.into_vec(),
            vec![
                "@corp/*",
                "new-glob",
                "@corp/pkg@2.0.1||1.2.2",
                "gone@1.0.0",
                "gone@1.0.0",
            ]
        );
        std::assert_matches!(
            tool.write_native(&project, &policy, false).await?,
            SyncReport::Unchanged { .. }
        );
        Ok(())
    }

    /// A pnpm project rooted at `root`, for the native sync tests.
    fn pnpm_sync_project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        }
    }

    /// A 14-day policy exempting `selectors`, with `admitted` as the evaluated admissions.
    fn pnpm_sync_policy(selectors: &[&str], admitted: Option<&[(&str, &str)]>) -> ResolvedPolicy {
        ResolvedPolicy {
            default_window: Some(cooldown_core::WindowSpec::MinAge(
                jiff::SignedDuration::from_hours(24 * 14),
            )),
            exempt_packages: selectors.iter().map(ToString::to_string).collect(),
            withheld_exemptions: Vec::new(),
            package_windows: std::collections::BTreeMap::new(),
            admitted_versions: admitted.map(|admitted| {
                admitted
                    .iter()
                    .map(|(name, version)| cooldown_core::AdmittedVersion {
                        name: (*name).to_string(),
                        version: Version::new(*version),
                    })
                    .collect()
            }),
        }
    }

    /// The selectors a sync report's `StricterNative` warnings name, in report order.
    fn omitted_selectors(report: &SyncReport) -> Vec<String> {
        let warnings = match report {
            SyncReport::Written { warnings, .. } | SyncReport::Unchanged { warnings, .. } => {
                warnings.as_slice()
            }
            SyncReport::Deferred { .. } | SyncReport::Unsupported => &[],
        };
        warnings
            .iter()
            .inspect(|warning| assert_eq!(warning.kind, DiagnosticKind::StricterNative))
            .filter_map(|warning| {
                let (_, rest) = warning.message.split_once('`')?;
                let (selector, _) = rest.split_once('`')?;
                Some(selector.to_string())
            })
            .collect()
    }

    fn pnpm_sync_tool() -> eyre::Result<(tempfile::TempDir, NpmTool<Pnpm>)> {
        let cache = tempfile::tempdir()?;
        let tool = NpmTool::<Pnpm>::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        Ok((cache, tool))
    }

    /// With complete admission evidence, sync writes the selectors pnpm reads as cooldown does,
    /// keeps an admitted exact union that only an omitted selector covers, and reports each
    /// omitted selector, in a dry run as in a real one.
    #[tokio::test]
    async fn write_native_writes_only_selectors_pnpm_reads_identically() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        std::fs::write(&path, "packages: []\n")?;
        let project = pnpm_sync_project(&root);
        let policy = pnpm_sync_policy(
            &[
                "@types/{node,react}",
                "*-plugin",
                "react-?om",
                "@types/*-utils",
            ],
            Some(&[
                ("@types/node", "22.0.0"),
                ("@types/react-utils", "1.0.0"),
                ("react-dom", "19.0.0"),
            ]),
        );
        let (_cache, tool) = pnpm_sync_tool()?;
        let omitted = ["@types/{node,react}", "react-?om"];

        // A dry run reports the write and the omissions without touching the file.
        let dry = tool.write_native(&project, &policy, true).await?;
        std::assert_matches!(dry, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&dry), omitted);
        assert_eq!(std::fs::read_to_string(&path)?, "packages: []\n");

        // The safe selectors come first; `@types/react-utils` is covered by the written
        // `@types/*-utils`, while `@types/node` and `react-dom` are covered only by omitted
        // selectors and keep their exact unions.
        let written = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(written, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&written), omitted);
        assert_eq!(
            NativeAge::read(&path)?.exclusions.into_vec(),
            [
                "*-plugin",
                "@types/*-utils",
                "@types/node@22.0.0",
                "react-dom@19.0.0"
            ]
        );

        // An unchanged second sync still reports the omissions.
        let again = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(again, SyncReport::Unchanged { .. });
        assert_eq!(omitted_selectors(&again), omitted);
        Ok(())
    }

    /// Without admission evidence, sync writes the safe selectors, keeps every existing exact
    /// entry verbatim, and still reports the omitted selectors.
    #[tokio::test]
    async fn write_native_omits_unsafe_selectors_when_evaluation_fails() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAgeExclude:
                  - "@types/{node,react}"
                  - "@types/node@22.0.0"
            "#},
        )?;
        let project = pnpm_sync_project(&root);
        let policy = pnpm_sync_policy(&["@types/{node,react}", "**", "*-plugin"], None);
        let (_cache, tool) = pnpm_sync_tool()?;

        let report = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(report, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&report), ["**", "@types/{node,react}"]);
        assert_eq!(
            NativeAge::read(&path)?.exclusions.into_vec(),
            ["*-plugin", "@types/node@22.0.0"]
        );
        Ok(())
    }

    /// A policy whose selectors are all omitted keeps an explicit empty list, so the exclusions
    /// pnpm inherits from user or global configuration stay masked; the list empties only once
    /// nothing is left in it, and it stays an explicit empty list when the policy drops its
    /// selectors, since removing it would expose the inherited exclusions again.
    #[tokio::test]
    async fn write_native_keeps_an_explicit_empty_list_for_omitted_selectors() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let explicit_empty = indoc! {"
            packages: []
            minimumReleaseAge: 20160
            minimumReleaseAgeExclude: []
        "};

        // Creating the key
        std::fs::write(&path, "packages: []\nminimumReleaseAge: 20160\n")?;
        let omitted_only = pnpm_sync_policy(&["react-?om"], Some(&[]));
        let created = tool.write_native(&project, &omitted_only, false).await?;
        std::assert_matches!(created, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&created), ["react-?om"]);
        assert_eq!(std::fs::read_to_string(&path)?, explicit_empty);

        // An unchanged second sync
        let again = tool.write_native(&project, &omitted_only, false).await?;
        std::assert_matches!(again, SyncReport::Unchanged { .. });
        assert_eq!(omitted_selectors(&again), ["react-?om"]);

        // Replacing an old list
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 20160
                minimumReleaseAgeExclude:
                  - "react-?om"
                  - "react-dom@19.0.0"
            "#},
        )?;
        tool.write_native(&project, &omitted_only, false).await?;
        assert_eq!(std::fs::read_to_string(&path)?, explicit_empty);

        // Preserved exact unions keep the list non-empty when evaluation fails.
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 20160
                minimumReleaseAgeExclude:
                  - "react-dom@19.0.0"
            "#},
        )?;
        let unevaluated = pnpm_sync_policy(&["react-?om"], None);
        std::assert_matches!(
            tool.write_native(&project, &unevaluated, false).await?,
            SyncReport::Unchanged { .. }
        );
        assert_eq!(
            NativeAge::read(&path)?.exclusions.into_vec(),
            ["react-dom@19.0.0"]
        );

        // Without selectors the explicit empty list is kept as it is.
        std::fs::write(&path, explicit_empty)?;
        let no_selectors = pnpm_sync_policy(&[], Some(&[]));
        let kept = tool.write_native(&project, &no_selectors, false).await?;
        std::assert_matches!(kept, SyncReport::Unchanged { .. });
        assert_eq!(omitted_selectors(&kept), [] as [String; 0]);
        assert_eq!(std::fs::read_to_string(&path)?, explicit_empty);
        Ok(())
    }

    /// With a writable window, a policy that names no selector empties an existing list to an
    /// explicit `[]` rather than removing it, which would let an inherited `["*"]` exempt every
    /// package; an absent key with nothing withheld or omitted stays absent.
    #[tokio::test]
    async fn write_native_never_removes_an_existing_list() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let no_selectors = pnpm_sync_policy(&[], Some(&[]));

        // An existing list
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 20160
                minimumReleaseAgeExclude:
                  - "foo"
            "#},
        )?;
        let emptied = tool.write_native(&project, &no_selectors, false).await?;
        std::assert_matches!(emptied, SyncReport::Written { .. });
        let explicit_empty = indoc! {"
            packages: []
            minimumReleaseAge: 20160
            minimumReleaseAgeExclude: []
        "};
        assert_eq!(std::fs::read_to_string(&path)?, explicit_empty);
        let again = tool.write_native(&project, &no_selectors, false).await?;
        std::assert_matches!(again, SyncReport::Unchanged { .. });

        // An absent key
        std::fs::write(&path, "packages: []\n")?;
        tool.write_native(&project, &no_selectors, false).await?;
        let written = std::fs::read_to_string(&path)?;
        assert_eq!(NativeAge::read(&path)?.minutes, Some(20160), "{written}");
        assert!(
            !written.contains("minimumReleaseAgeExclude"),
            "an absent key stays absent: {written}"
        );
        Ok(())
    }

    /// Under a freeze-only policy, cleanup keeps an existing explicit empty list and empties a
    /// pruned `["foo"]` to `[]`, so an inherited `["*"]` never becomes active; an absent key with
    /// nothing withheld or omitted stays absent.
    #[tokio::test]
    async fn write_native_cleanup_never_removes_an_existing_list() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let mut freeze_only = pnpm_sync_policy(&[], Some(&[]));
        freeze_only.default_window = Some(cooldown_core::WindowSpec::Freeze(
            "2026-10-31T00:00:00Z".parse()?,
        ));
        let explicit_empty = indoc! {"
            packages: []
            minimumReleaseAge: 10080
            minimumReleaseAgeExclude: []
        "};

        // An existing explicit empty list
        std::fs::write(&path, explicit_empty)?;
        let kept = tool.write_native(&project, &freeze_only, false).await?;
        std::assert_matches!(kept, SyncReport::Unchanged { .. });
        assert_eq!(std::fs::read_to_string(&path)?, explicit_empty);

        // A list the cleanup empties
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 10080
                minimumReleaseAgeExclude:
                  - "foo"
            "#},
        )?;
        let emptied = tool.write_native(&project, &freeze_only, false).await?;
        std::assert_matches!(emptied, SyncReport::Written { .. });
        assert_eq!(std::fs::read_to_string(&path)?, explicit_empty);
        let again = tool.write_native(&project, &freeze_only, false).await?;
        std::assert_matches!(again, SyncReport::Unchanged { .. });

        // An absent key
        let absent = indoc! {"
            packages: []
            minimumReleaseAge: 10080
        "};
        std::fs::write(&path, absent)?;
        let untouched = tool.write_native(&project, &freeze_only, false).await?;
        std::assert_matches!(untouched, SyncReport::Unchanged { .. });
        assert_eq!(std::fs::read_to_string(&path)?, absent);
        Ok(())
    }

    /// A window pnpm cannot express leaves `minimumReleaseAge` alone and still reports omitted
    /// selectors; an omitted selector adds an explicit empty list, masking inherited exclusions,
    /// while a policy whose selectors are all written leaves the file untouched.
    #[tokio::test]
    async fn write_native_reports_omitted_selectors_when_the_window_is_not_written()
    -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        std::fs::write(&path, "packages: []\n")?;
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;

        let mut written_only = pnpm_sync_policy(&["*-plugin"], Some(&[]));
        written_only.default_window = Some(cooldown_core::WindowSpec::Latest);
        let untouched = tool.write_native(&project, &written_only, false).await?;
        std::assert_matches!(untouched, SyncReport::Unchanged { .. });
        assert_eq!(std::fs::read_to_string(&path)?, "packages: []\n");

        let mut policy = pnpm_sync_policy(&["[!a-c]*", "*-plugin"], Some(&[]));
        policy.default_window = Some(cooldown_core::WindowSpec::Latest);
        let report = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(report, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&report), ["[!a-c]*"]);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            indoc! {"
                packages: []
                minimumReleaseAgeExclude: []
            "}
        );
        Ok(())
    }

    /// A 14-day policy that withholds `withheld` and exempts nothing package-wide.
    fn pnpm_withholding_policy(
        withheld: &[&str],
        admitted: Option<&[(&str, &str)]>,
    ) -> ResolvedPolicy {
        let mut policy = pnpm_sync_policy(&[], admitted);
        policy.withheld_exemptions = withheld.iter().map(ToString::to_string).collect();
        policy
    }

    /// A withheld selector replaces an existing package-wide entry with an explicit empty list,
    /// so inherited exclusions stay masked, and each sync names it in a warning that says why.
    #[tokio::test]
    async fn write_native_withholds_a_selector_behind_an_explicit_empty_list() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 20160
                minimumReleaseAgeExclude:
                  - "@corp/*"
            "#},
        )?;
        let project = pnpm_sync_project(&root);
        let policy = pnpm_withholding_policy(&["@corp/*"], Some(&[]));
        let (_cache, tool) = pnpm_sync_tool()?;

        let written = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(written, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&written), ["@corp/*"]);
        let SyncReport::Written { warnings, .. } = &written else {
            eyre::bail!("expected a written report: {written:?}");
        };
        assert!(
            warnings.iter().all(|warning| warning
                .message
                .contains("cannot prove that every release `@corp/*` matches is exempt")),
            "{warnings:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path)?,
            indoc! {"
                packages: []
                minimumReleaseAge: 20160
                minimumReleaseAgeExclude: []
            "}
        );

        // A second sync is unchanged and still names the withheld selector.
        let again = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(again, SyncReport::Unchanged { .. });
        assert_eq!(omitted_selectors(&again), ["@corp/*"]);
        Ok(())
    }

    /// The young locked versions cooldown admits under a withheld selector are written as exact
    /// unions instead of the package-wide entry, and a re-sync leaves them unchanged.
    #[tokio::test]
    async fn write_native_admits_exact_versions_under_a_withheld_selector() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        std::fs::write(&path, "packages: []\n")?;
        let project = pnpm_sync_project(&root);
        let policy = pnpm_withholding_policy(
            &["@corp/*"],
            Some(&[("@corp/ui", "2.0.0"), ("@corp/ui", "1.0.0")]),
        );
        let (_cache, tool) = pnpm_sync_tool()?;

        let written = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(written, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&written), ["@corp/*"]);
        assert_eq!(
            NativeAge::read(&path)?.exclusions.into_vec(),
            ["@corp/ui@1.0.0||2.0.0"]
        );
        let again = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(again, SyncReport::Unchanged { .. });
        Ok(())
    }

    /// Without admission evidence, a withheld selector's old package-wide entry is still dropped
    /// while the existing exact entries survive verbatim.
    #[tokio::test]
    async fn write_native_drops_a_withheld_selector_when_evaluation_fails() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 20160
                minimumReleaseAgeExclude:
                  - "@corp/*"
                  - "@corp/ui@1.0.0"
            "#},
        )?;
        let project = pnpm_sync_project(&root);
        let policy = pnpm_withholding_policy(&["@corp/*"], None);
        let (_cache, tool) = pnpm_sync_tool()?;

        let written = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(written, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&written), ["@corp/*"]);
        assert_eq!(
            NativeAge::read(&path)?.exclusions.into_vec(),
            ["@corp/ui@1.0.0"]
        );
        Ok(())
    }

    /// A window pnpm cannot express leaves `minimumReleaseAge` as it is, yet the broad entry of a
    /// withheld selector is still dropped, since that stale window keeps gating: otherwise `["*"]`
    /// would keep admitting brand-new majors cooldown holds back.
    /// Exact entries survive verbatim, and the list empties to an explicit `[]`.
    #[tokio::test]
    async fn write_native_prunes_a_withheld_entry_when_the_window_is_not_written()
    -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let freeze = cooldown_core::WindowSpec::Freeze("2026-01-01T00:00:00Z".parse()?);
        for window in [cooldown_core::WindowSpec::Latest, freeze] {
            std::fs::write(
                &path,
                indoc! {r#"
                    packages: []
                    minimumReleaseAge: 10080
                    minimumReleaseAgeExclude:
                      - "*"
                      - "@corp/ui@1.0.0"
                "#},
            )?;
            let mut policy = pnpm_withholding_policy(&["*"], Some(&[]));
            policy.default_window = Some(window.clone());

            // A dry run reports the cleanup without touching the file.
            let dry = tool.write_native(&project, &policy, true).await?;
            std::assert_matches!(dry, SyncReport::Written { .. }, "{window:?}");

            let written = tool.write_native(&project, &policy, false).await?;
            std::assert_matches!(written, SyncReport::Written { .. }, "{window:?}");
            assert_eq!(omitted_selectors(&written), ["*"]);
            assert_eq!(
                std::fs::read_to_string(&path)?,
                indoc! {r#"
                    packages: []
                    minimumReleaseAge: 10080
                    minimumReleaseAgeExclude:
                      - "@corp/ui@1.0.0"
                "#},
                "{window:?}"
            );
            let again = tool.write_native(&project, &policy, false).await?;
            std::assert_matches!(again, SyncReport::Unchanged { .. }, "{window:?}");
        }

        // A list holding only the withheld entry becomes an explicit empty list.
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 10080
                minimumReleaseAgeExclude:
                  - "*"
            "#},
        )?;
        let mut policy = pnpm_withholding_policy(&["*"], Some(&[]));
        policy.default_window = Some(cooldown_core::WindowSpec::Latest);
        tool.write_native(&project, &policy, false).await?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            indoc! {"
                packages: []
                minimumReleaseAge: 10080
                minimumReleaseAgeExclude: []
            "}
        );
        Ok(())
    }

    /// Without a writable window, the cleanup keeps written selectors and drops omitted and
    /// withheld ones.
    #[tokio::test]
    async fn write_native_prunes_unwritten_selectors_when_the_window_is_not_written()
    -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let mut policy = pnpm_sync_policy(&["react-?om", "*-plugin"], Some(&[]));
        policy.default_window = Some(cooldown_core::WindowSpec::Latest);
        policy.withheld_exemptions = vec!["@corp/*".to_string()];
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAgeExclude:
                  - "*-plugin"
                  - "react-?om"
                  - "@corp/*"
            "#},
        )?;

        let pruned = tool.write_native(&project, &policy, false).await?;

        std::assert_matches!(pruned, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&pruned), ["@corp/*", "react-?om"]);
        assert_eq!(NativeAge::read(&path)?.exclusions.into_vec(), ["*-plugin"]);
        Ok(())
    }

    /// Under a freeze, a withheld selector masks inherited exclusions with an explicit empty list
    /// even where the key, or the whole file, was absent, as a sync with a writable window would,
    /// and a re-sync leaves it unchanged.
    #[tokio::test]
    async fn write_native_masks_inherited_exclusions_when_the_window_is_not_written()
    -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let mut policy = pnpm_withholding_policy(&["@corp/*"], Some(&[]));
        policy.default_window = Some(cooldown_core::WindowSpec::Freeze(
            "2026-10-31T00:00:00Z".parse()?,
        ));

        // An absent key
        std::fs::write(
            &path,
            indoc! {"
                packages: []
                minimumReleaseAge: 10080
            "},
        )?;
        let written = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(written, SyncReport::Written { .. });
        assert_eq!(omitted_selectors(&written), ["@corp/*"]);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            indoc! {"
                packages: []
                minimumReleaseAge: 10080
                minimumReleaseAgeExclude: []
            "}
        );
        let again = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(again, SyncReport::Unchanged { .. });

        // An absent file
        std::fs::remove_file(&path)?;
        tool.write_native(&project, &policy, false).await?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "minimumReleaseAgeExclude: []\n"
        );
        Ok(())
    }

    /// A scalar setting is read one rule per character, as pnpm reads it, unless pnpm may expand
    /// an environment variable in it first, which leaves it alone.
    #[tokio::test]
    async fn write_native_prunes_a_scalar_setting_only_when_it_can_read_it() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let mut policy = pnpm_withholding_policy(&["*"], Some(&[]));
        policy.default_window = Some(cooldown_core::WindowSpec::Latest);

        let expanded = indoc! {r#"
            packages: []
            minimumReleaseAgeExclude: "${EXEMPT}"
        "#};
        std::fs::write(&path, expanded)?;
        let untouched = tool.write_native(&project, &policy, false).await?;
        std::assert_matches!(untouched, SyncReport::Unchanged { .. });
        assert_eq!(std::fs::read_to_string(&path)?, expanded);

        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAgeExclude: "*"
            "#},
        )?;
        tool.write_native(&project, &policy, false).await?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            indoc! {"
                packages: []
                minimumReleaseAgeExclude: []
            "}
        );
        Ok(())
    }

    /// Pruning keeps every version-qualified rule in any spelling pnpm accepts, so no version moves
    /// between rules: `foo@v1.0.0` keeps shadowing `foo@2.0.0`, and an exact rule keeps shadowing
    /// the name-wide rule after it, which goes.
    #[tokio::test]
    async fn write_native_pruning_keeps_every_version_qualified_rule() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        let project = pnpm_sync_project(&root);
        let (_cache, tool) = pnpm_sync_tool()?;
        let mut policy = pnpm_withholding_policy(&["@corp/*"], Some(&[]));
        policy.default_window = Some(cooldown_core::WindowSpec::Freeze(
            "2026-10-31T00:00:00Z".parse()?,
        ));
        std::fs::write(
            &path,
            indoc! {r#"
                packages: []
                minimumReleaseAge: 10080
                minimumReleaseAgeExclude:
                  - "foo@v1.0.0"
                  - "foo@2.0.0"
                  - "bar@1.0.0 || 2.0.0"
                  - "baz@1.0.0"
                  - "baz"
                  - "@corp/*"
            "#},
        )?;
        let before = NativeAge::read(&path)?.exclusions.into_vec();

        tool.write_native(&project, &policy, false).await?;

        let after = NativeAge::read(&path)?.exclusions.into_vec();
        assert_eq!(
            after,
            ["foo@v1.0.0", "foo@2.0.0", "bar@1.0.0 || 2.0.0", "baz@1.0.0"]
        );
        for name in ["foo", "bar", "baz", "@corp/ui"] {
            for version in ["1.0.0", "2.0.0", "3.0.0"] {
                assert!(
                    !crate::exclusions::exempts(&after, name, version)
                        || crate::exclusions::exempts(&before, name, version),
                    "pruning widened {name}@{version}"
                );
            }
        }
        Ok(())
    }

    /// Withheld selectors are reported before omitted ones, and both keep the list explicit.
    #[tokio::test]
    async fn write_native_reports_withheld_and_omitted_selectors() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("pnpm-workspace.yaml");
        std::fs::write(&path, "packages: []\n")?;
        let project = pnpm_sync_project(&root);
        let mut policy = pnpm_sync_policy(&["react-?om", "*-plugin"], Some(&[]));
        policy.withheld_exemptions = vec!["@corp/*".to_string()];
        let (_cache, tool) = pnpm_sync_tool()?;

        let written = tool.write_native(&project, &policy, false).await?;
        assert_eq!(omitted_selectors(&written), ["@corp/*", "react-?om"]);
        assert_eq!(NativeAge::read(&path)?.exclusions.into_vec(), ["*-plugin"]);
        Ok(())
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
        assert_ne!(detail, "");
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

        let hold = graph_ceiling_hold::<Pnpm>(&advance, after, true)
            .expect("a parsable settlement lock yields the hold");
        let detail = hold.detail.expect("advanced holds carry a detail");
        assert!(
            detail.contains("holds it at 3.4.9"),
            "the settled copy, not the stale pre-apply version, is named: {detail}"
        );

        // An empty document is a legitimately empty lock, not a parse failure: the name is simply
        // absent, so the pre-apply version is the honest fallback.
        let unresolvable =
            graph_ceiling_hold::<Pnpm>(&advance, "", true).expect("an empty lock is parsable");
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

        let error = graph_ceiling_hold::<Pnpm>(&advance, malformed, true)
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let mut landed = change("chalk", "4.1.1", "4.1.2");
        landed.members = vec![cooldown_core::MemberRef {
            name: "root".to_string(),
            path: ".".to_string(),
        }];

        assert!(
            exact_target_reached::<Npm>(&project, &landed, None)?,
            "the root instance sits at the target; the nested 5.6.0 duplicate must not mask it"
        );
        let mut short = change("chalk", "4.1.1", "4.1.3");
        short.members.clone_from(&landed.members);
        assert!(
            !exact_target_reached::<Npm>(&project, &short, None)?,
            "an instance short of its target is still short"
        );
        Ok(())
    }

    /// An npm alias at a member's slot installs another package, so its version proves no landing
    /// of the name even when it equals the target and a genuine copy at the target sits elsewhere
    /// in the tree; with several members, every one must resolve a genuine copy at the target.
    #[test]
    fn an_npm_alias_never_proves_a_landing() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let lock = |member_slot: &str| {
            indoc::formatdoc! {r#"
                {{
                  "name": "root",
                  "lockfileVersion": 3,
                  "packages": {{
                    "": {{ "name": "root", "workspaces": ["apps/a", "apps/b"] }},
                    "apps/a": {{ "name": "a", "version": "0.1.0" }},
                    "apps/b": {{ "name": "b", "version": "0.1.0" }},
                    {member_slot}
                    "node_modules/x": {{ "version": "1.0.0" }},
                    "node_modules/x/node_modules/chalk": {{ "version": "4.1.2" }}
                  }}
                }}
            "#}
        };
        let member = |path: &str| cooldown_core::MemberRef {
            name: path.to_string(),
            path: path.to_string(),
        };
        let mut landed = change("chalk", "4.1.1", "4.1.2");
        landed.members = vec![member("apps/a")];

        // The alias's version equals the target, and a genuine 4.1.2 is nested under `x`.
        std::fs::write(
            root.join("package-lock.json"),
            lock(r#""node_modules/chalk": { "name": "other", "version": "4.1.2" },"#),
        )?;
        assert!(!exact_target_reached::<Npm>(&project, &landed, None)?);

        // A member bound to nothing proves nothing either, though the newest copy is at the target.
        std::fs::write(root.join("package-lock.json"), lock(""))?;
        assert!(!exact_target_reached::<Npm>(&project, &landed, None)?);

        // One member reaches a genuine copy, the other only an alias.
        std::fs::write(
            root.join("package-lock.json"),
            lock(indoc::indoc! {r#"
                "node_modules/chalk": { "version": "4.1.2" },
                "apps/b/node_modules/chalk": { "name": "other", "version": "4.1.2" },
            "#}),
        )?;
        let mut both = landed.clone();
        both.members.push(member("apps/b"));
        assert!(exact_target_reached::<Npm>(&project, &landed, None)?);
        assert!(!exact_target_reached::<Npm>(&project, &both, None)?);
        Ok(())
    }

    /// A lock with a genuine `other` and an alias `foo` installing another release of `other`,
    /// as a v1 `dependencies` tree or a v3 install tree.
    fn genuine_and_alias_lock(lockfile_version: u8, genuine: &str, aliased: &str) -> String {
        if lockfile_version == 1 {
            indoc::formatdoc! {r#"
                {{
                  "name": "root",
                  "lockfileVersion": 1,
                  "dependencies": {{
                    "foo": {{ "version": "npm:other@{aliased}" }},
                    "other": {{ "version": "{genuine}" }}
                  }}
                }}
            "#}
        } else {
            indoc::formatdoc! {r#"
                {{
                  "name": "root",
                  "lockfileVersion": {lockfile_version},
                  "packages": {{
                    "": {{ "name": "root" }},
                    "node_modules/foo": {{ "name": "other", "version": "{aliased}" }},
                    "node_modules/other": {{ "version": "{genuine}" }}
                  }}
                }}
            "#}
        }
    }

    /// A change without declaring members, every change of a v1 lock, is judged on the name's
    /// newest copy, which must leave out a release only an alias installs.
    /// Otherwise an alias at the target proves a move the genuine copy never made, and a newer
    /// alias hides a genuine copy that did land, rolling a successful move back.
    #[test]
    fn an_alias_target_neither_proves_nor_masks_a_memberless_landing() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let mut planned = change("other", "1.0.0", "1.1.0");
        planned.direct = false;
        for lockfile_version in [1, 3] {
            let reached = |genuine: &str, aliased: &str| -> eyre::Result<bool> {
                std::fs::write(
                    root.join("package-lock.json"),
                    genuine_and_alias_lock(lockfile_version, genuine, aliased),
                )?;
                Ok(exact_target_reached::<Npm>(&project, &planned, None)?)
            };

            // The genuine copy stayed behind while the alias sits at the target.
            assert!(!reached("1.0.0", "1.1.0")?, "v{lockfile_version}");
            // The genuine copy landed beneath a newer alias.
            assert!(reached("1.1.0", "1.2.0")?, "v{lockfile_version}");
            // A genuine copy of the alias's release makes that release ordinary evidence again.
            assert!(reached("1.1.0", "1.1.0")?, "v{lockfile_version}");
        }
        Ok(())
    }

    /// A release only an alias installs has no declaration to land through: the root's exact pin
    /// of the target's name belongs to the genuine copy, so a transitive move of the alias's
    /// release is skipped as not eligible before any manifest edit or install, rather than
    /// rewriting that unrelated pin.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_alias_only_release_never_lands_through_the_root_declaration() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        for lockfile_version in [1, 3] {
            let dir = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
                .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
            let manifest = indoc! {r#"
                { "name": "root", "dependencies": { "foo": "npm:other@^1.0.0", "other": "1.0.0" } }
            "#};
            let lock = genuine_and_alias_lock(lockfile_version, "1.0.0", "1.2.0");
            std::fs::write(root.join("package.json"), manifest)?;
            std::fs::write(root.join("package-lock.json"), &lock)?;
            // Every invocation is logged, and an install fails, so a landing attempt is visible.
            let script = root.join("fake-npm.sh");
            std::fs::write(
                &script,
                indoc! {r#"
                    #!/bin/sh
                    echo "$*" >> legs.log
                    echo "unexpected args: $*" >&2; exit 1
                "#},
            )?;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
            let mut tool = tool();
            tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
            let mut planned = change("other", "1.2.0", "1.1.0");
            planned.direct = false;
            planned.downgrade = true;
            let plan = Plan {
                changes: vec![planned],
                rewrite: RewriteMode::Auto,
                ..Plan::default()
            };

            let mutation = PreparedMutation::prepare(&tool, &npm_project(&root), &plan).await?;
            let report = tool.apply(&mutation).await?;

            // The alias's release is skipped with the reason naming the alias.
            assert!(report.applied.is_empty(), "v{lockfile_version}: {report:?}");
            let [skip] = report.skipped.as_slice() else {
                eyre::bail!("v{lockfile_version}: one skip: {:?}", report.skipped)
            };
            assert_eq!(skip.reason, SkipReason::NotEligible, "v{lockfile_version}");
            assert_eq!(
                skip.detail.as_deref(),
                Some(
                    "other@1.2.0 is installed only through an npm alias, so no declaration of \
                     other owns it"
                ),
                "v{lockfile_version}"
            );
            // Neither the root's exact pin nor the lock was touched, and npm never ran an install.
            assert_eq!(
                std::fs::read_to_string(root.join("package.json"))?,
                manifest,
                "v{lockfile_version}"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("package-lock.json"))?,
                lock,
                "v{lockfile_version}"
            );
            let legs = std::fs::read_to_string(root.join("legs.log")).unwrap_or_default();
            assert!(!legs.contains("install"), "v{lockfile_version}: {legs}");
        }
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let landed = change("chalk", "4.1.1", "4.1.2");

        let error = exact_target_reached::<Npm>(&project, &landed, None)
            .expect_err("an unparsable lock is an error, not an unreached target");
        assert!(
            matches!(error, CoreError::Parse(_)),
            "npm's strict lock parse error propagates, got: {error:?}"
        );
        Ok(())
    }

    /// How the fake pnpm's targeted `update` leaves the lock in [`write_floated_lock_fixture`].
    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum Float {
        /// `dep` floats out of its declared `^1.0.0` to `2.0.0`.
        OutOfRange,
        /// `dep` lands at its `1.1.0` target, but the unrelated exact pin `pinned: 1.0.0` floats
        /// to `1.2.0`, within its major, which only an npm-grammar check sees.
        UnrelatedExactPin,
    }

    /// Writes the floated-lock fixture: a manifest declaring `dep: ^1.0.0` beside the exact pin
    /// `pinned: 1.0.0`, a consistent seed lock, the floated and repaired lock shapes, and a
    /// scripted fake pnpm whose `update` leg floats an importer entry out of its declared range
    /// (the shape pnpm was observed to produce under `--no-save`) while the override-engine legs
    /// repair it. Returns the script path to inject via [`crate::nodecmd::NodeCmd::with_bin`].
    #[cfg(unix)]
    fn write_floated_lock_fixture(root: &Utf8Path, float: Float) -> eyre::Result<Utf8PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "app", "dependencies": { "dep": "^1.0.0", "pinned": "1.0.0" } }"#,
        )?;
        let lock = |dep: &str, pinned: &str| {
            indoc::formatdoc! {"
                lockfileVersion: '9.0'

                importers:

                  .:
                    dependencies:
                      dep:
                        specifier: ^1.0.0
                        version: {dep}
                      pinned:
                        specifier: 1.0.0
                        version: {pinned}

                packages:

                  dep@{dep}:
                    resolution: {{integrity: sha512-a}}

                  pinned@{pinned}:
                    resolution: {{integrity: sha512-b}}
            "}
        };
        let floated = match float {
            Float::OutOfRange => lock("2.0.0", "1.0.0"),
            Float::UnrelatedExactPin => lock("1.1.0", "1.2.0"),
        };
        std::fs::write(root.join("pnpm-lock.yaml"), lock("1.0.0", "1.0.0"))?;
        std::fs::write(root.join("floated.yaml"), floated)?;
        std::fs::write(root.join("repaired.yaml"), lock("1.1.0", "1.0.0"))?;
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
        repairs_floated_lock(Float::OutOfRange).await
    }

    /// An unrelated exact pin that the targeted `update` floats within its major (`1.0.0` to
    /// `1.2.0`) is a fresh inconsistency too.
    /// The repair runs and the committed lock holds the pin at its version again.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_floated_unrelated_exact_pin_is_repaired_through_the_override_engine()
    -> eyre::Result<()> {
        repairs_floated_lock(Float::UnrelatedExactPin).await
    }

    /// Runs the floated-lock fixture for `float` and asserts the override repair lands `dep` and
    /// commits the repaired lock.
    #[cfg(unix)]
    async fn repairs_floated_lock(float: Float) -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let script = write_floated_lock_fixture(&root, float)?;
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            .apply_whole_graph(
                &project,
                &plan,
                &journal,
                &evidence,
                &ProtectedPins::default(),
            )
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
        let committed = std::fs::read_to_string(root.join("pnpm-lock.yaml"))?;
        assert!(
            committed.contains("dep@1.1.0") && committed.contains("pinned@1.0.0"),
            "the repaired lock, with the exact pin back at its version, is the committed result: \
             {committed}"
        );
        Ok(())
    }

    /// An interruption while pnpm resolves under the temporary overrides stops the resolver and
    /// restores the user's `pnpm-workspace.yaml` byte for byte, comments included, instead of
    /// leaving the merged pins behind.
    ///
    /// The interruption flag is process-global; nextest runs every test in its own process, so it
    /// cannot leak into another test.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_interrupted_override_resolve_restores_the_workspace_config() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let script = write_floated_lock_fixture(&root, Float::OutOfRange)?;
        let workspace = indoc! {"
            # Exact pins the codegen toolchain needs.
            overrides:
              # Keep the generated code and its runtime in lockstep.
              pinned: 1.0.0
        "};
        std::fs::write(root.join("pnpm-workspace.yaml"), workspace)?;
        // The resolution-only install announces itself and then blocks, as a hung resolver does,
        // until the interruption terminates it.
        std::fs::write(
            &script,
            indoc! {r#"
                #!/bin/sh
                case "$*" in
                  *"config get"*)
                    echo 'null'; exit 0 ;;
                  *" update "*)
                    cp floated.yaml pnpm-lock.yaml; exit 0 ;;
                  *"--resolution-only"*)
                    touch resolving
                    exec sleep 60 ;;
                esac
                echo "unexpected args: $*" >&2; exit 1
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
        let protected_pins = ProtectedPins::default();
        let apply = tool.apply_whole_graph(&project, &plan, &journal, &evidence, &protected_pins);
        tokio::pin!(apply);

        // Interrupt only once the temporary overrides are on disk and the resolver is running.
        let interrupted = async {
            while !root.join("resolving").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            let merged = std::fs::read_to_string(root.join("pnpm-workspace.yaml"))?;
            assert!(
                merged.contains("dep@^1.0.0"),
                "the resolver runs under the temporary pins: {merged}"
            );
            cooldown_core::interrupt::request();
            eyre::Ok(())
        };
        let (result, signalled) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            tokio::join!(&mut apply, interrupted)
        })
        .await?;
        signalled?;

        // The run fails as interrupted rather than as a resolver conflict on the candidate.
        let Err(error) = result else {
            eyre::bail!("an interrupted resolve must fail the apply");
        };
        assert!(
            error.to_string().contains("interrupted"),
            "the failure names the interruption: {error}"
        );
        assert!(
            error.is_local_environment_failure(),
            "resilient apply must propagate it, not isolate a candidate"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("pnpm-workspace.yaml"))?,
            workspace,
            "the user's overrides block is restored with its comments"
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            .apply_whole_graph(
                &project,
                &plan,
                &journal,
                &evidence,
                &ProtectedPins::default(),
            )
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let plan = Plan {
            changes: vec![change("nanoid", "3.1.0", "3.3.0")],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };

        let tool = tool();
        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;

        assert_eq!(report.applied, []);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].reason, SkipReason::NotEligible);
        let manifest = std::fs::read_to_string(root.join("package.json"))?;
        assert_eq!(manifest, r#"{ "name": "root" }"#);
        Ok(())
    }

    /// Writes a `dom`/`safe` npm project pinned at `1.0.0` and a scripted `npm` for it,
    /// returning the script path.
    ///
    /// The pin leg rewrites the pinned entry's lock version; the major is refused as a peer
    /// conflict, the way npm's resolver rejects it, and the resync leg keeps the pins.
    #[cfg(unix)]
    fn current_line_fallback_fixture(root: &Utf8Path) -> eyre::Result<Utf8PathBuf> {
        use std::os::unix::fs::PermissionsExt as _;
        let manifest = indoc! {r#"
            { "name": "app", "dependencies": { "dom": "^1.0.0", "safe": "^1.0.0" } }
        "#};
        std::fs::write(root.join("package.json"), manifest)?;
        let lock = indoc! {r#"
            {
                "name": "app",
                "lockfileVersion": 3,
                "packages": {
                    "": { "name": "app", "dependencies": { "dom": "^1.0.0", "safe": "^1.0.0" } },
                    "node_modules/dom": { "version": "1.0.0" },
                    "node_modules/safe": { "version": "1.0.0" }
                }
            }
        "#};
        std::fs::write(root.join("package-lock.json"), lock)?;
        let script = root.join("fake-npm.sh");
        std::fs::write(
            &script,
            indoc! {r#"
                #!/bin/sh
                echo "$*" >> legs.log
                case "$*" in
                  "install dom@2.0.0 "*)
                    echo "npm error code ERESOLVE" >&2; exit 1 ;;
                  "install --package-lock-only"*)
                    exit 0 ;;
                  "install "*@*" --package-lock-only"*)
                    name="${2%@*}"; version="${2##*@}"
                    sed "s|\"node_modules/$name\": { \"version\": \"[^\"]*\"|\"node_modules/$name\": { \"version\": \"$version\"|" \
                      package-lock.json > package-lock.next && mv package-lock.next package-lock.json
                    exit 0 ;;
                esac
                echo "unexpected args: $*" >&2; exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        Ok(script)
    }

    /// The per-package npm path supports the executor's current-line fallback: a refused major
    /// target leaves no manifest edit behind beside a landed companion, and the in-line target
    /// retried on top of that companion from the restored baseline lands exactly with it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_major_restores_its_manifest_and_its_current_line_target_lands()
    -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let script = current_line_fallback_fixture(&root)?;
        let mut tool = tool();
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        assert_eq!(tool.target_fallback(), TargetFallback::CurrentLine);
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let trial = |changes| Plan {
            changes,
            ..Plan::default()
        };
        let locked = |name: &str| -> eyre::Result<Option<String>> {
            let content = std::fs::read_to_string(root.join("package-lock.json"))?;
            Ok(locked_versions::<Npm>(&content)?.get(name).cloned())
        };

        // Initial trial: the major is refused, the companion lands.
        let initial = trial(vec![
            change("dom", "1.0.0", "2.0.0"),
            change("safe", "1.0.0", "1.1.0"),
        ]);
        let baseline = tool.mutation_journal(&project, &initial).await?;
        let mutation = PreparedMutation::prepare(&tool, &project, &initial).await?;
        let report = tool.apply(&mutation).await?;
        assert_eq!(
            report
                .applied
                .iter()
                .map(|change| format!("{}@{}", change.package.name, change.to))
                .collect::<Vec<_>>(),
            ["safe@1.1.0"]
        );
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].change.to.as_str(), "2.0.0");
        let declared = std::fs::read_to_string(root.join("package.json"))?;
        assert!(declared.contains(r#""dom": "^1.0.0""#), "{declared}");
        assert_eq!(locked("dom")?.as_deref(), Some("1.0.0"));
        assert_eq!(locked("safe")?.as_deref(), Some("1.1.0"));

        // Current-line attempt from the restored baseline, on top of the accepted companion.
        baseline.restore()?;
        let retry = trial(vec![
            change("dom", "1.0.0", "1.1.0"),
            change("safe", "1.0.0", "1.1.0"),
        ]);
        let mutation = PreparedMutation::prepare(&tool, &project, &retry).await?;
        let report = tool.apply(&mutation).await?;
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let mut landed: Vec<String> = report
            .applied
            .iter()
            .map(|change| format!("{}@{}", change.package.name, change.to))
            .collect();
        landed.sort();
        assert_eq!(landed, ["dom@1.1.0", "safe@1.1.0"]);
        assert_eq!(locked("dom")?.as_deref(), Some("1.1.0"));
        assert_eq!(locked("safe")?.as_deref(), Some("1.1.0"));
        // The in-line target stays on the major line the manifest declares.
        let declared = std::fs::read_to_string(root.join("package.json"))?;
        assert!(!declared.contains("^2"), "{declared}");
        Ok(())
    }

    /// npm holds a candidate on its exact `devDependencies` pin, with the pin named,
    /// while the unrelated sibling in the same batch lands, and no npm command targets the held
    /// candidate.
    #[cfg(unix)]
    #[tokio::test]
    async fn npm_holds_a_hidden_pin_and_lands_its_sibling() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let script = current_line_fallback_fixture(&root)?;
        // npm's root lock entry mirrors both manifest fields, so the lock sees the pin as one of
        // two declarations and the dependency is not pinned as a whole.
        let manifest = indoc! {r#"
            {
              "name": "app",
              "dependencies": { "dom": "^1.0.0", "safe": "^1.0.0" },
              "devDependencies": { "dom": "1.0.0" }
            }
        "#};
        std::fs::write(root.join("package.json"), manifest)?;
        let lock = std::fs::read_to_string(root.join("package-lock.json"))?.replace(
            r#""dependencies": { "dom": "^1.0.0", "safe": "^1.0.0" } }"#,
            r#""dependencies": { "dom": "^1.0.0", "safe": "^1.0.0" }, "devDependencies": { "dom": "1.0.0" } }"#,
        );
        assert!(lock.contains("devDependencies"), "{lock}");
        std::fs::write(root.join("package-lock.json"), lock)?;
        let mut tool = tool();
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        // The lock sees the pin beside the caret, so the dependency is not pinned as a whole and the
        // planner may move it.
        let deps = tool.dependencies(&project, DepScope::Direct).await?;
        assert!(
            deps.iter()
                .any(|dep| dep.package.name == "dom" && !dep.pinned),
            "{deps:?}"
        );
        let plan = Plan {
            changes: vec![
                change("dom", "1.0.0", "1.1.0"),
                change("safe", "1.0.0", "1.1.0"),
            ],
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;

        assert_eq!(
            report
                .applied
                .iter()
                .map(|change| format!("{}@{}", change.package.name, change.to))
                .collect::<Vec<_>>(),
            ["safe@1.1.0"]
        );
        let [skip] = report.skipped.as_slice() else {
            eyre::bail!("one held candidate: {:?}", report.skipped)
        };
        assert_eq!(skip.change.package.name, "dom");
        assert_eq!(skip.reason, SkipReason::PinHeld);
        assert_eq!(
            skip.detail.as_deref(),
            Some(
                "the exact pin 1.0.0 in devDependencies of package.json excludes 1.1.0; \
                 pass --rewrite to rewrite it, or edit the pin"
            )
        );
        let declared = std::fs::read_to_string(root.join("package.json"))?;
        assert!(declared.contains(r#""dom": "^1.0.0""#), "{declared}");
        assert!(
            declared.contains(r#""devDependencies": { "dom": "1.0.0" }"#),
            "{declared}"
        );
        let legs = std::fs::read_to_string(root.join("legs.log"))?;
        assert!(!legs.contains("dom@"), "{legs}");
        Ok(())
    }

    /// npm holds a `v`-prefixed or spaced hidden pin up front like a bare one: a lone held
    /// candidate runs no npm command at all and leaves every byte in place, and `--rewrite` lands
    /// it on an exact pin of the target, never a caret.
    #[cfg(unix)]
    #[tokio::test]
    async fn npm_holds_a_prefixed_hidden_pin_without_running_npm() -> eyre::Result<()> {
        for (prefix, rewritten) in [("v", ""), ("=v", "="), ("= v", "=")] {
            let dir = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
                .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
            let script = current_line_fallback_fixture(&root)?;
            let pin = format!("{prefix}1.0.0");
            let manifest = indoc::formatdoc! {r#"
                {{
                  "name": "app",
                  "dependencies": {{ "dom": "^1.0.0", "safe": "^1.0.0" }},
                  "devDependencies": {{ "dom": "{pin}" }}
                }}
            "#};
            std::fs::write(root.join("package.json"), &manifest)?;
            let lock = std::fs::read_to_string(root.join("package-lock.json"))?.replace(
                r#""dependencies": { "dom": "^1.0.0", "safe": "^1.0.0" } }"#,
                &format!(
                    r#""dependencies": {{ "dom": "^1.0.0", "safe": "^1.0.0" }}, "devDependencies": {{ "dom": "{pin}" }} }}"#
                ),
            );
            assert!(lock.contains("devDependencies"), "{lock}");
            std::fs::write(root.join("package-lock.json"), &lock)?;
            let mut tool = tool();
            tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
            let project = Project {
                root: root.clone(),
                kind: Npm::ID,
                manifest: root.join("package.json"),
                exclude_newer: None,
                exclude_newer_package: std::collections::BTreeMap::new(),
                generated_members: cooldown_core::GeneratedMembers::undeclared(),
            };
            let plan = |rewrite| Plan {
                changes: vec![change("dom", "1.0.0", "1.1.0")],
                rewrite,
                ..Plan::default()
            };

            let mutation =
                PreparedMutation::prepare(&tool, &project, &plan(RewriteMode::Auto)).await?;
            let report = tool.apply(&mutation).await?;

            assert_eq!(report.applied, [], "{pin}");
            let [skip] = report.skipped.as_slice() else {
                eyre::bail!("{pin}: one held candidate: {:?}", report.skipped)
            };
            assert_eq!(skip.reason, SkipReason::PinHeld, "{pin}");
            assert_eq!(
                skip.detail.as_deref(),
                Some(
                    format!(
                        "the exact pin {pin} in devDependencies of package.json excludes 1.1.0; \
                         pass --rewrite to rewrite it, or edit the pin"
                    )
                    .as_str()
                )
            );
            assert_eq!(
                std::fs::read_to_string(root.join("package.json"))?,
                manifest
            );
            assert_eq!(
                std::fs::read_to_string(root.join("package-lock.json"))?,
                lock
            );
            assert!(
                !root.join("legs.log").exists(),
                "{pin}: a lone held candidate runs no npm command"
            );

            let mutation =
                PreparedMutation::prepare(&tool, &project, &plan(RewriteMode::Always)).await?;
            let report = tool.apply(&mutation).await?;

            assert!(report.skipped.is_empty(), "{pin}: {:?}", report.skipped);
            assert_eq!(
                report
                    .applied
                    .iter()
                    .map(|change| format!("{}@{}", change.package.name, change.to))
                    .collect::<Vec<_>>(),
                ["dom@1.1.0"]
            );
            let declared = std::fs::read_to_string(root.join("package.json"))?;
            assert!(
                declared.contains(&format!(
                    r#""devDependencies": {{ "dom": "{rewritten}1.1.0" }}"#
                )),
                "{declared}"
            );
            assert!(declared.contains(r#""dom": "^1.1.0""#), "{declared}");
        }
        Ok(())
    }

    /// An npm root declaring `deps` at carets and `pinned` exactly in `devDependencies`, every
    /// package locked at 1.0.0, driven by a scripted npm whose exact pin of a name lands it at the
    /// requested version.
    /// The pin of `second`, and any pin while `moves-pinned` exists, also floats `pinned` to
    /// 1.1.0, the way npm's install can re-resolve a sibling the manifest pins.
    /// A pin carrying a `--before` cutoff fails as an unresolvable historical tree.
    #[cfg(unix)]
    fn sibling_pin_fixture(
        root: &Utf8Path,
        manifest: &str,
        names: &[&str],
    ) -> eyre::Result<NpmTool<Npm>> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(root.join("package.json"), manifest)?;
        let packages = names
            .iter()
            .map(|name| format!(r#""node_modules/{name}": {{ "version": "1.0.0" }}"#))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            root.join("package-lock.json"),
            indoc::formatdoc! {r#"
                {{
                    "name": "app",
                    "lockfileVersion": 3,
                    "packages": {{ "": {{ "name": "app" }}, {packages} }}
                }}
            "#},
        )?;
        let script = root.join("fake-npm.sh");
        std::fs::write(
            &script,
            indoc! {r#"
                #!/bin/sh
                echo "$*" >> legs.log
                set_version() {
                    sed "s|\"node_modules/$1\": { \"version\": \"[^\"]*\"|\"node_modules/$1\": { \"version\": \"$2\"|" \
                      package-lock.json > package-lock.next && mv package-lock.next package-lock.json
                }
                case "$*" in
                  "install --package-lock-only"*)
                    exit 0 ;;
                  "install "*@*" --package-lock-only"*"--before="*)
                    echo "npm error code ETARGET" >&2; exit 1 ;;
                  "install "*@*" --package-lock-only"*)
                    name="${2%@*}"; version="${2##*@}"
                    set_version "$name" "$version"
                    if [ "$name" = second ] || [ -f moves-pinned ]; then set_version pinned 1.1.0; fi
                    exit 0 ;;
                esac
                echo "unexpected args: $*" >&2; exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let mut tool = tool();
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        Ok(tool)
    }

    #[cfg(unix)]
    fn npm_project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        }
    }

    /// npm lands candidates one at a time, so each is judged against the state the earlier
    /// landings left: the first lands, the second's install floats a pinned sibling and alone is
    /// rolled back, and the lock keeps exactly the first candidate's result.
    #[cfg(unix)]
    #[tokio::test]
    async fn npm_rolls_back_only_the_candidate_that_moves_a_pin() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let tool = sibling_pin_fixture(
            &root,
            indoc! {r#"
                {
                  "name": "app",
                  "dependencies": { "first": "^1.0.0", "second": "^1.0.0", "pinned": "^1.0.0" },
                  "devDependencies": { "pinned": "1.0.0" }
                }
            "#},
            &["first", "second", "pinned"],
        )?;
        let baseline = std::fs::read_to_string(root.join("package-lock.json"))?;
        let plan = Plan {
            changes: vec![
                change("first", "1.0.0", "1.1.0"),
                change("second", "1.0.0", "1.1.0"),
            ],
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &npm_project(&root), &plan).await?;
        let report = tool.apply(&mutation).await?;

        assert_eq!(
            report
                .applied
                .iter()
                .map(|change| format!("{}@{}", change.package.name, change.to))
                .collect::<Vec<_>>(),
            ["first@1.1.0"]
        );
        let [skip] = report.skipped.as_slice() else {
            eyre::bail!("one rolled-back candidate: {:?}", report.skipped)
        };
        assert_eq!(skip.change.package.name, "second");
        assert_eq!(skip.reason, SkipReason::ResolverConflict);
        assert_eq!(
            skip.detail.as_deref(),
            Some(
                "the resolve moved an exact pin it was not asked to rewrite: pinned resolves to \
                 1.1.0 for package.json, whose devDependencies pins 1.0.0"
            )
        );
        // The lock is the first candidate's post-state, byte for byte.
        let first_landed = baseline.replace(
            r#""node_modules/first": { "version": "1.0.0" }"#,
            r#""node_modules/first": { "version": "1.1.0" }"#,
        );
        assert_eq!(
            std::fs::read_to_string(root.join("package-lock.json"))?,
            first_landed
        );
        let declared = std::fs::read_to_string(root.join("package.json"))?;
        assert!(declared.contains(r#""second": "^1.0.0""#), "{declared}");
        assert!(
            declared.contains(r#""devDependencies": { "pinned": "1.0.0" }"#),
            "{declared}"
        );
        Ok(())
    }

    /// A `--rewrite` pin edit is recorded when cooldown writes it, so it survives npm's retry
    /// without the cutoff and the authorized-manifest restoration before it: the candidate's own
    /// rewritten pin lands, while an unrelated pin the retry floats is still refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn npm_keeps_a_rewrite_authority_across_the_cutoff_retry() -> eyre::Result<()> {
        let manifest = indoc! {r#"
            {
              "name": "app",
              "dependencies": { "own": "^1.0.0", "pinned": "^1.0.0" },
              "devDependencies": { "own": "1.0.0", "pinned": "1.0.0" }
            }
        "#};
        let plan = Plan {
            changes: vec![change("own", "1.0.0", "1.1.0")],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };
        let run = |moves_pinned: bool| {
            let plan = plan.clone();
            async move {
                let dir = tempfile::tempdir()?;
                let root =
                    Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).map_err(|path| {
                        eyre::eyre!("temporary path is not UTF-8: {}", path.display())
                    })?;
                let tool = sibling_pin_fixture(&root, manifest, &["own", "pinned"])?;
                if moves_pinned {
                    std::fs::write(root.join("moves-pinned"), "")?;
                }
                let project = Project {
                    exclude_newer: Some("2024-01-01T00:00:00Z".to_string()),
                    ..npm_project(&root)
                };
                let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
                let report = tool.apply(&mutation).await?;
                let legs = std::fs::read_to_string(root.join("legs.log"))?;
                let declared = std::fs::read_to_string(root.join("package.json"))?;
                eyre::Ok((report, legs, declared))
            }
        };

        let (landed, legs, declared) = run(false).await?;
        assert_eq!(
            landed
                .applied
                .iter()
                .map(|change| change.package.name.as_str())
                .collect::<Vec<_>>(),
            ["own"]
        );
        assert!(landed.skipped.is_empty(), "{:?}", landed.skipped);
        assert!(
            legs.contains("--before="),
            "the first pin carried the cutoff: {legs}"
        );
        assert!(
            declared.contains(r#""devDependencies": { "own": "1.1.0", "pinned": "1.0.0" }"#),
            "{declared}"
        );

        let (refused, _, declared) = run(true).await?;
        assert!(refused.applied.is_empty(), "{:?}", refused.applied);
        let [skip] = refused.skipped.as_slice() else {
            eyre::bail!("one rolled-back candidate: {:?}", refused.skipped)
        };
        assert_eq!(skip.reason, SkipReason::ResolverConflict);
        assert_eq!(
            skip.detail.as_deref(),
            Some(
                "the resolve moved an exact pin it was not asked to rewrite: pinned resolves to \
                 1.1.0 for package.json, whose devDependencies pins 1.0.0"
            )
        );
        assert!(
            declared.contains(r#""devDependencies": { "own": "1.0.0", "pinned": "1.0.0" }"#),
            "{declared}"
        );
        Ok(())
    }

    /// A Bun workspace whose root declares `first`, `second`, and `pinned` at carets and whose
    /// excluded-to-be member `packages/a` (`pkg-a`) pins `pinned` exactly, kept at the member's
    /// nested key below the root's newer copy, driven by a scripted `bun install` that lands each
    /// widened root caret at 1.1.0.
    /// Landing `second` also drops the member's nested `pinned`, the way Bun dedupes a nested key
    /// onto the root copy, so the member's binding falls back to `pinned@1.1.0`.
    #[cfg(unix)]
    fn bun_sibling_pin_fixture(root: &Utf8Path) -> eyre::Result<NpmTool<Bun>> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(
            root.join("package.json"),
            indoc! {r#"
                {
                  "name": "app",
                  "workspaces": ["packages/*"],
                  "dependencies": { "first": "^1.0.0", "second": "^1.0.0", "pinned": "^1.0.0" }
                }
            "#},
        )?;
        std::fs::create_dir_all(root.join("packages/a"))?;
        std::fs::write(
            root.join("packages/a/package.json"),
            r#"{ "name": "pkg-a", "dependencies": { "pinned": "1.0.0" } }"#,
        )?;
        std::fs::write(
            root.join("bun.lock"),
            indoc! {r#"
                {
                  "lockfileVersion": 1,
                  "workspaces": {
                    "": {
                      "name": "app",
                      "dependencies": {
                        "first": "^1.0.0",
                        "pinned": "^1.0.0",
                        "second": "^1.0.0",
                      },
                    },
                    "packages/a": {
                      "name": "pkg-a",
                      "dependencies": {
                        "pinned": "1.0.0",
                      },
                    },
                  },
                  "packages": {
                    "first": ["first@1.0.0", "", {}, "sha512-x"],

                    "pinned": ["pinned@1.1.0", "", {}, "sha512-x"],

                    "pkg-a": ["pkg-a@workspace:packages/a"],

                    "second": ["second@1.0.0", "", {}, "sha512-x"],

                    "pkg-a/pinned": ["pinned@1.0.0", "", {}, "sha512-x"],
                  }
                }
            "#},
        )?;
        let script = root.join("fake-bun.sh");
        std::fs::write(
            &script,
            indoc! {r#"
                #!/bin/sh
                echo "$*" >> legs.log
                land() {
                    sed -e "s|\"$1\": \[\"$1@1.0.0\"|\"$1\": [\"$1@1.1.0\"|" \
                        -e "s|\"$1\": \"^1.0.0\"|\"$1\": \"^1.1.0\"|" \
                      bun.lock > bun.next && mv bun.next bun.lock
                }
                case "$*" in
                  install)
                    if grep -q '"first": "^1.1.0"' package.json; then land first; fi
                    if grep -q '"second": "^1.1.0"' package.json; then
                      land second
                      grep -v '"pkg-a/pinned"' bun.lock > bun.next && mv bun.next bun.lock
                    fi
                    exit 0 ;;
                esac
                echo "unexpected args: $*" >&2; exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let cache = tempfile::tempdir()?;
        let mut tool = NpmTool::<Bun>::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        Ok(tool)
    }

    fn bun_project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: Bun::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        }
    }

    /// A Bun change declared by `members`, given as `(name, path)`.
    fn bun_change(name: &str, from: &str, to: &str, members: &[(&str, &str)]) -> Change {
        Change {
            package: PackageId::new(Bun::ID, name, Some(NPM.to_string())),
            members: members
                .iter()
                .map(|(name, path)| MemberRef {
                    name: (*name).to_string(),
                    path: (*path).to_string(),
                })
                .collect(),
            ..change(name, from, to)
        }
    }

    /// Bun lands candidates one at a time, judging each against the state the earlier landings
    /// left: the first lands, while the second's install moves an excluded member's exact pin and
    /// alone is rolled back, so the lock keeps exactly the first candidate's result.
    #[cfg(unix)]
    #[tokio::test]
    async fn bun_rolls_back_only_the_candidate_that_moves_an_excluded_members_pin()
    -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let tool = bun_sibling_pin_fixture(&root)?;
        let baseline = std::fs::read_to_string(root.join("bun.lock"))?;
        let plan = Plan {
            changes: vec![
                bun_change("first", "1.0.0", "1.1.0", &[("app", ".")]),
                bun_change("second", "1.0.0", "1.1.0", &[("app", ".")]),
            ],
            excluded_members: vec![MemberRef {
                name: "pkg-a".to_string(),
                path: "packages/a".to_string(),
            }],
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &bun_project(&root), &plan).await?;
        let report = tool.apply(&mutation).await?;

        // The accepted candidate stays applied.
        assert_eq!(
            report
                .applied
                .iter()
                .map(|change| format!("{}@{}", change.package.name, change.to))
                .collect::<Vec<_>>(),
            ["first@1.1.0"]
        );

        // The later candidate is refused on the excluded member's pin, which it alone moved.
        let [skip] = report.skipped.as_slice() else {
            eyre::bail!("one rolled-back candidate: {:?}", report.skipped)
        };
        assert_eq!(skip.change.package.name, "second");
        assert_eq!(skip.reason, SkipReason::ResolverConflict);
        assert_eq!(
            skip.detail.as_deref(),
            Some(
                "the resolve moved an exact pin it was not asked to rewrite: pinned resolves to \
                 1.1.0 for packages/a/package.json, whose dependencies pins 1.0.0"
            )
        );

        // The lock and manifests are the first candidate's post-state, the member's nested copy
        // and the second caret restored.
        let first_landed = baseline
            .replace(r#""first@1.0.0""#, r#""first@1.1.0""#)
            .replace(r#""first": "^1.0.0""#, r#""first": "^1.1.0""#);
        assert_eq!(
            std::fs::read_to_string(root.join("bun.lock"))?,
            first_landed
        );
        let declared = std::fs::read_to_string(root.join("package.json"))?;
        assert!(declared.contains(r#""first": "^1.1.0""#), "{declared}");
        assert!(declared.contains(r#""second": "^1.0.0""#), "{declared}");
        Ok(())
    }

    /// Bun's landing is judged at each declaring member's own binding: a newer copy another member
    /// binds never stands in for the member's own, an alias is never landed by a copy of the name
    /// elsewhere, and only a member whose own copy the lock leaves undetermined falls back to the
    /// name's newest copy.
    #[test]
    fn bun_landing_is_verified_at_each_declaring_member() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let project = bun_project(&root);
        let lock = |member: &str, nested: &str| {
            indoc::formatdoc! {r#"
                {{
                  "lockfileVersion": 2,
                  "workspaces": {{
                    "": {{ "name": "app", "dependencies": {{ "chalk": "^4.1.0" }} }},
                    "packages/b": {{ "name": "pkg-b", "dependencies": {{ "chalk": "{member}" }} }},
                  }},
                  "packages": {{
                    "chalk": ["chalk@4.1.2", "", {{}}, "sha512-x"],
                    "pkg-b": ["pkg-b@workspace:packages/b"],
                    {nested}
                  }},
                }}
            "#}
        };
        let to = |version: &str| bun_change("chalk", "4.1.0", version, &[("pkg-b", "packages/b")]);

        // The member keeps its nested copy below the root's newer one.
        let nested = r#""pkg-b/chalk": ["chalk@4.1.0", "", {}, "sha512-x"],"#;
        std::fs::write(root.join("bun.lock"), lock("4.1.0", nested))?;
        assert!(!exact_target_reached::<Bun>(&project, &to("4.1.2"), None)?);
        assert!(exact_target_reached::<Bun>(&project, &to("4.1.0"), None)?);

        // The member binds the root copy once its nested one is gone.
        std::fs::write(root.join("bun.lock"), lock("^4.1.0", ""))?;
        assert!(exact_target_reached::<Bun>(&project, &to("4.1.2"), None)?);

        // An alias is no copy of the name, so the root's copy at the target never lands it.
        let aliased = r#""pkg-b/chalk": ["other@4.1.0", "", {}, "sha512-x"],"#;
        std::fs::write(root.join("bun.lock"), lock("npm:other@4.1.0", aliased))?;
        assert!(!exact_target_reached::<Bun>(&project, &to("4.1.2"), None)?);
        assert!(!exact_target_reached::<Bun>(&project, &to("4.1.0"), None)?);

        // A member that also declares the name as a peer binds a copy the lock leaves
        // undetermined, so the name's newest copy is the evidence, as before Bun members were
        // attributed.
        let peer = lock("^4.1.0", "").replace(
            r#""dependencies": { "chalk": "^4.1.0" } },"#,
            r#""dependencies": { "chalk": "^4.1.0" }, "peerDependencies": { "chalk": "^4.0.0" } },"#,
        );
        assert!(peer.contains("peerDependencies"), "{peer}");
        std::fs::write(root.join("bun.lock"), &peer)?;
        assert!(exact_target_reached::<Bun>(&project, &to("4.1.2"), None)?);
        assert!(!exact_target_reached::<Bun>(&project, &to("4.1.0"), None)?);

        // A member with a known binding beside it is still judged on its own copy.
        let both = Change {
            members: vec![
                MemberRef {
                    name: "pkg-b".to_string(),
                    path: "packages/b".to_string(),
                },
                MemberRef {
                    name: "pkg-c".to_string(),
                    path: "packages/c".to_string(),
                },
            ],
            ..to("4.1.2")
        };
        let mixed = peer.replace(
            r#""pkg-b": ["pkg-b@workspace:packages/b"],"#,
            indoc! {r#"
                "pkg-b": ["pkg-b@workspace:packages/b"],
                "pkg-c": ["pkg-c@workspace:packages/c"],
                "pkg-c/chalk": ["chalk@4.1.0", "", {}, "sha512-x"],
            "#},
        );
        let mixed = mixed.replace(
            r#""packages/b": {"#,
            r#""packages/c": { "name": "pkg-c", "dependencies": { "chalk": "4.1.0" } }, "packages/b": {"#,
        );
        std::fs::write(root.join("bun.lock"), &mixed)?;
        assert_eq!(
            Bun::member_attribution(&mixed).resolution("packages/c", "chalk"),
            MemberResolution::Resolved("4.1.0")
        );
        assert!(!exact_target_reached::<Bun>(&project, &both, None)?);
        Ok(())
    }

    /// The Bun inventory attributes each resolved copy to the members binding it, so a version
    /// only an alias installs is no member's, and an exact copy every declaration pins is held.
    #[tokio::test]
    async fn bun_inventory_attributes_each_member_its_own_copy() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("bun.lock"), crate::lock::bun_fixtures::WORKSPACES)?;
        // The manifests the fixture's workspace entries copy.
        for (path, manifest) in [
            (
                "",
                r#"{ "name": "root", "workspaces": ["packages/*"], "dependencies": { "chalk": "4.1.2", "is-number": "^7.0.0" } }"#,
            ),
            (
                "packages/a",
                r#"{ "name": "pkg-a", "dependencies": { "chalk": "4.1.0", "@types/node": "20.0.0" } }"#,
            ),
            (
                "packages/b",
                r#"{ "name": "pkg-b", "dependencies": { "chalk": "4.1.2", "pkg-a": "workspace:*", "isnum": "npm:is-number@6.0.0" } }"#,
            ),
            (
                "packages/scoped",
                r#"{ "name": "@acme/scoped", "devDependencies": { "chalk": "5.3.0", "@types/node": "20.1.0" } }"#,
            ),
        ] {
            std::fs::create_dir_all(root.join(path))?;
            std::fs::write(root.join(path).join("package.json"), manifest)?;
        }
        let tool = NpmTool::<Bun>::from_http(SharedHttp::new(
            dir.path().join("cache").as_path(),
            cooldown_registry::HttpOptions::default(),
        )?);

        let deps = tool
            .dependencies(&bun_project(&root), DepScope::Graph)
            .await?;
        let mut rows: Vec<String> = deps
            .iter()
            .filter(|dep| dep.direct)
            .map(|dep| {
                let members = dep
                    .members
                    .iter()
                    .map(|member| member.name.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                let pinned = if dep.pinned { " pinned" } else { "" };
                format!("{}@{} [{members}]{pinned}", dep.package.name, dep.current)
            })
            .collect();
        rows.sort();

        // Each copy belongs to its own members; the alias's target and every transitive are no
        // member's.
        assert_eq!(
            rows,
            [
                "@types/node@20.0.0 [pkg-a] pinned",
                "@types/node@20.1.0 [@acme/scoped] pinned",
                "chalk@4.1.0 [pkg-a] pinned",
                "chalk@4.1.2 [root,pkg-b] pinned",
                "chalk@5.3.0 [@acme/scoped] pinned",
                "is-number@7.0.0 [root]",
            ]
        );
        assert!(
            deps.iter().any(|dep| dep.package.name == "is-number"
                && dep.current.as_str() == "6.0.0"
                && !dep.direct),
            "{deps:?}"
        );
        Ok(())
    }

    /// A git identity's inner `@` must not make a registry package: the lock's `pkg` is a git
    /// dependency, so neither a row for it nor one for its `pkg@git+ssh://git` split reaches
    /// evaluation, while the registry copy beside it still does.
    #[tokio::test]
    async fn bun_git_identity_is_not_a_registry_package() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("bun.lock"),
            indoc! {r#"
                {
                    "lockfileVersion": 1,
                    "workspaces": {
                        "": {
                            "name": "root",
                            "dependencies": {
                                "chalk": "4.1.2",
                                "pkg": "git+ssh://git@github.com/x/pkg.git#abc123",
                            },
                        },
                    },
                    "packages": {
                        "chalk": ["chalk@4.1.2", "", {}, "sha512-a"],
                        "pkg": ["pkg@git+ssh://git@github.com/x/pkg.git#abc123", {}],
                    },
                }"#},
        )?;
        std::fs::write(
            root.join("package.json"),
            indoc! {r#"
                {
                    "name": "root",
                    "dependencies": {
                        "chalk": "4.1.2",
                        "pkg": "git+ssh://git@github.com/x/pkg.git#abc123"
                    }
                }"#},
        )?;
        let tool = NpmTool::<Bun>::from_http(SharedHttp::new(
            dir.path().join("cache").as_path(),
            cooldown_registry::HttpOptions::default(),
        )?);

        let deps = tool
            .dependencies(&bun_project(&root), DepScope::Graph)
            .await?;
        let rows: Vec<String> = deps
            .iter()
            .map(|dep| format!("{}@{}", dep.package.name, dep.current.as_str()))
            .collect();
        assert_eq!(rows, ["chalk@4.1.2"]);
        Ok(())
    }

    /// A name the root and a member each declare both as a dev dependency and as a peer stays
    /// direct, each copy attributed to the member whose key records it and bounded by its
    /// declarations, though the lock leaves which copy each finally binds unknown.
    #[tokio::test]
    async fn bun_peer_and_dev_declarations_stay_direct() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("bun.lock"),
            crate::lock::bun_fixtures::ROOT_PEER_AND_DEV,
        )?;
        // The manifests the fixture's workspace entries copy.
        for (path, manifest) in [
            (
                "",
                r#"{ "name": "root", "workspaces": ["packages/*"], "devDependencies": { "chalk": ">=4.1.0 <4.2.0" }, "peerDependencies": { "chalk": "^4.0.0" } }"#,
            ),
            (
                "packages/a",
                r#"{ "name": "pkg-a", "devDependencies": { "chalk": "4.1.0" }, "peerDependencies": { "chalk": ">=4.0.0 <5.0.0" } }"#,
            ),
        ] {
            std::fs::create_dir_all(root.join(path))?;
            std::fs::write(root.join(path).join("package.json"), manifest)?;
        }
        let tool = NpmTool::<Bun>::from_http(SharedHttp::new(
            dir.path().join("cache").as_path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let project = bun_project(&root);
        let rows = |deps: &[Dependency]| {
            let mut rows: Vec<String> = deps
                .iter()
                .filter(|dep| dep.package.name == "chalk")
                .map(|dep| {
                    let members = dep
                        .members
                        .iter()
                        .map(|member| member.name.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    let bound = if dep.declared_bound.is_some() {
                        " bounded"
                    } else {
                        ""
                    };
                    format!(
                        "chalk@{} direct={} [{members}]{bound}",
                        dep.current, dep.direct
                    )
                })
                .collect();
            rows.sort();
            rows
        };
        // Each member owns the copy its own key records; the member's exact dev pin is no upper
        // bound, while the root's range is.
        let expected = [
            "chalk@4.1.0 direct=true [pkg-a]",
            "chalk@4.1.2 direct=true [root] bounded",
        ];

        // Both copies are direct rows of the upgrade inventory.
        let direct = tool.dependencies(&project, DepScope::Direct).await?;
        assert_eq!(rows(&direct), expected);

        // The graph inventory classifies them direct too, beside a transitive that stays one.
        let graph = tool.dependencies(&project, DepScope::Graph).await?;
        assert_eq!(rows(&graph), expected);
        assert!(
            graph
                .iter()
                .any(|dep| dep.package.name == "ansi-styles" && !dep.direct),
            "{graph:?}"
        );
        Ok(())
    }

    /// A project on the [`ALIAS_BESIDE_TRANSITIVE`](crate::lock::bun_fixtures::ALIAS_BESIDE_TRANSITIVE)
    /// lock with the manifests its workspace entries copy, the cache under `root`.
    fn bun_alias_project(root: &Utf8Path) -> eyre::Result<NpmTool<Bun>> {
        std::fs::write(
            root.join("bun.lock"),
            crate::lock::bun_fixtures::ALIAS_BESIDE_TRANSITIVE,
        )?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root", "workspaces": ["packages/*"], "dependencies": { "log-symbols": "4.1.0" } }"#,
        )?;
        std::fs::create_dir_all(root.join("packages/b"))?;
        std::fs::write(
            root.join("packages/b/package.json"),
            r#"{ "name": "pkg-b", "dependencies": { "chalk": "npm:is-number@6.0.0" } }"#,
        )?;
        Ok(NpmTool::<Bun>::from_http(SharedHttp::new(
            root.join(".cache").as_std_path(),
            cooldown_registry::HttpOptions::default(),
        )?))
    }

    /// A member aliasing a name to another package owns none of the name's registry copies, so the
    /// root's transitive copy stays a memberless transitive row, which excluding the member cannot
    /// prune, while the member's declaration is still shown.
    #[tokio::test]
    async fn bun_alias_owns_no_registry_copy_of_its_name() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let tool = bun_alias_project(&root)?;
        let project = bun_project(&root);

        // The transitive copy is no member's, whether or not the member is excluded.
        let graph = tool.dependencies(&project, DepScope::Graph).await?;
        let Some(chalk) = graph
            .iter()
            .find(|dep| dep.package.name == "chalk" && dep.current.as_str() == "4.1.2")
        else {
            eyre::bail!("the transitive chalk row is missing: {graph:?}")
        };
        assert!(!chalk.direct, "{chalk:?}");
        assert_eq!(chalk.members, []);
        let excluded = HashSet::from(["packages/b".to_string()]);
        let content = std::fs::read_to_string(root.join("bun.lock"))?;
        assert_eq!(
            Bun::member_sources_excluding(&content, &excluded).members_for("chalk", "4.1.2"),
            Bun::member_sources(&content).members_for("chalk", "4.1.2"),
        );

        // The member's own declaration is still shown, bound to no registry version.
        let declarations = tool.declarations(&project, "chalk").await?;
        let [declaration] = declarations.as_slice() else {
            eyre::bail!("one declaration: {declarations:?}")
        };
        assert_eq!(declaration.member.path, "packages/b");
        assert_eq!(declaration.range.as_deref(), Some("npm:is-number@6.0.0"));
        assert_eq!(declaration.resolved, None);
        Ok(())
    }

    /// An install that changes nothing never lands a candidate whose only declaring member aliases
    /// the name, even though a registry copy of the name elsewhere already sits at the target.
    #[cfg(unix)]
    #[tokio::test]
    async fn bun_alias_is_not_landed_by_an_unrelated_copy() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let mut tool = bun_alias_project(&root)?;
        let script = root.join("fake-bun.sh");
        std::fs::write(
            &script,
            indoc! {r#"
                #!/bin/sh
                echo "$*" >> legs.log
                [ "$*" = install ] && exit 0
                echo "unexpected args: $*" >&2; exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        let lock = std::fs::read_to_string(root.join("bun.lock"))?;
        let plan = Plan {
            changes: vec![bun_change(
                "chalk",
                "4.1.0",
                "4.1.2",
                &[("pkg-b", "packages/b")],
            )],
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &bun_project(&root), &plan).await?;
        let report = tool.apply(&mutation).await?;

        // The no-op install ran, yet nothing is reported applied.
        assert!(std::fs::read_to_string(root.join("legs.log"))?.contains("install"));
        assert_eq!(report.applied, []);
        let [skip] = report.skipped.as_slice() else {
            eyre::bail!("one held candidate: {:?}", report.skipped)
        };
        assert_eq!(skip.change.package.name, "chalk");
        assert_eq!(std::fs::read_to_string(root.join("bun.lock"))?, lock);
        Ok(())
    }

    /// A member declaring a name as a dev range and a peer owns only the copy its key records, so
    /// the root's transitive copy of another line stays a memberless transitive row that excluding
    /// the member cannot prune, while the member's own copy stays direct with its bound.
    #[tokio::test]
    async fn bun_uncertain_member_owns_no_transitive_line() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let lock = crate::lock::bun_fixtures::PEER_BESIDE_TRANSITIVE;
        std::fs::write(root.join("bun.lock"), lock)?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "root", "workspaces": ["packages/*"], "dependencies": { "log-symbols": "6.0.0" } }"#,
        )?;
        std::fs::create_dir_all(root.join("packages/b"))?;
        std::fs::write(
            root.join("packages/b/package.json"),
            r#"{ "name": "pkg-b", "devDependencies": { "chalk": ">=4.1.0 <4.1.1" }, "peerDependencies": { "chalk": "^4.0.0" } }"#,
        )?;
        let tool = NpmTool::<Bun>::from_http(SharedHttp::new(
            root.join(".cache").as_std_path(),
            cooldown_registry::HttpOptions::default(),
        )?);

        let graph = tool
            .dependencies(&bun_project(&root), DepScope::Graph)
            .await?;
        let row = |version: &str| {
            graph
                .iter()
                .find(|dep| dep.package.name == "chalk" && dep.current.as_str() == version)
        };

        // The transitive line is no member's, so no exclusion can prune it.
        let Some(transitive) = row("5.6.2") else {
            eyre::bail!("the transitive chalk row is missing: {graph:?}")
        };
        assert!(!transitive.direct, "{transitive:?}");
        assert_eq!(transitive.members, []);

        // The member's own copy is direct, attributed to it, and bounded by its range.
        let Some(own) = row("4.1.0") else {
            eyre::bail!("the member's chalk row is missing: {graph:?}")
        };
        assert!(own.direct, "{own:?}");
        assert_eq!(
            own.members
                .iter()
                .map(|member| member.path.as_str())
                .collect::<Vec<_>>(),
            ["packages/b"]
        );
        assert_eq!(own.declared_bound.as_deref(), Some(">=4.1.0 <4.1.1"));

        // Excluding the member leaves the transitive line untouched.
        let excluded = HashSet::from(["packages/b".to_string()]);
        assert_eq!(
            Bun::member_sources_excluding(lock, &excluded).members_for("chalk", "5.6.2"),
            Bun::member_sources(lock).members_for("chalk", "5.6.2"),
        );
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
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
        // as a nested mapping, and a comparator range (`'>=20.0.0'`), whose `>` would open a
        // folded block scalar.
        let specifier = if specifier.contains(':') || specifier.starts_with(['>', '<']) {
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

    /// A scripted `pnpm` that logs every invocation to `legs.log`, answers `config get` with an
    /// empty exclusion list or `null`, and otherwise runs `body` (a `case "$*"` arm list).
    /// An unmatched invocation fails.
    fn fake_pnpm(root: &Utf8Path, body: &str) -> eyre::Result<Utf8PathBuf> {
        fake_pnpm_with_exclusions(root, body, &[])
    }

    fn fake_pnpm_with_exclusions(
        root: &Utf8Path,
        body: &str,
        exclusions: &[&str],
    ) -> eyre::Result<Utf8PathBuf> {
        let exclusions = serde_json::to_string(exclusions)?;
        fake_pnpm_answering_exclusions(root, body, &format!("echo '{exclusions}'; exit 0"))
    }

    /// Like [`fake_pnpm`], with `exclusion_answer` as the shell answering the exclusion query.
    fn fake_pnpm_answering_exclusions(
        root: &Utf8Path,
        body: &str,
        exclusion_answer: &str,
    ) -> eyre::Result<Utf8PathBuf> {
        let script = root.join("fake-pnpm.sh");
        std::fs::write(
            &script,
            formatdoc! {r#"
                #!/bin/sh
                echo "$*" >> legs.log
                case "$*" in
                  *"config get minimumReleaseAgeExclude"*)
                    {exclusion_answer} ;;
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

    fn single_direct_dependency(root: &Utf8Path) -> eyre::Result<(String, Plan)> {
        let lock = workspace(
            root,
            &[Importer {
                path: ".",
                name: "app",
                deps: vec![("foo", "^1.0.0", "1.0.0")],
            }],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let plan = Plan {
            changes: vec![change("foo", "1.0.0", "1.1.0", &[("app", ".")])],
            ..Plan::default()
        };
        Ok((lock, plan))
    }

    /// A release-age exclusion query pnpm itself fails is a broken environment, not a resolver
    /// verdict, so the apply fails before the update instead of holding the candidate.
    #[tokio::test]
    async fn a_failed_exclusion_query_fails_the_apply_before_the_update() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let (lock, plan) = single_direct_dependency(&root)?;
        let script = fake_pnpm_answering_exclusions(
            &root,
            "",
            "echo 'ERR_PNPM_CONFIG unreadable' >&2; exit 1",
        )?;
        let tool = tool_with(&script)?;

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("an unreadable configuration must not be reported as held candidates");

        assert!(error.is_local_environment_failure(), "{error:?}");
        assert!(
            error.to_string().contains("minimumReleaseAgeExclude"),
            "{error}"
        );
        assert_eq!(legs(&root), [] as [String; 0]);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        Ok(())
    }

    /// The repair leg reads the exclusion setting too, and a failed read there is the same broken
    /// environment: a transitive-only plan takes that leg, and the apply fails before any install.
    #[tokio::test]
    async fn a_failed_exclusion_query_fails_the_repair_leg_before_the_install() -> eyre::Result<()>
    {
        let (_dir, root) = tempdir_root()?;
        let mut lock = workspace(
            &root,
            &[Importer {
                path: ".",
                name: "app",
                deps: vec![("parent", "^1.0.0", "1.0.0")],
            }],
        )?;
        lock.push_str(&package_entry("foo", "1.0.0"));
        lock.push_str(indoc! {"

            snapshots:
              parent@1.0.0:
                dependencies:
                  foo: 1.0.0
        "});
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let script = fake_pnpm_answering_exclusions(
            &root,
            "",
            "echo 'ERR_PNPM_CONFIG unreadable' >&2; exit 1",
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![Change {
                direct: false,
                ..change("foo", "1.0.0", "1.1.0", &[])
            }],
            ..Plan::default()
        };

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("an unreadable configuration must not be reported as held candidates");

        assert!(error.is_local_environment_failure(), "{error:?}");
        assert!(
            error.to_string().contains("minimumReleaseAgeExclude"),
            "{error}"
        );
        assert_eq!(legs(&root), [] as [String; 0]);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        Ok(())
    }

    /// pnpm reads a scalar exclusion setting as one pattern per character, so the update carries
    /// those patterns as its list and adds the pin only where none of them already exempts it.
    #[tokio::test]
    async fn a_scalar_exclusion_setting_keeps_pnpms_per_character_reading() -> eyre::Result<()> {
        for (scalar, expected) in [
            // No single character names `foo`, so its pin is appended.
            ("lodash", "l o d a s h foo@1.1.0"),
            // The `*` character already exempts every package, `foo` included.
            ("@corp/*", "@ c o r p / *"),
        ] {
            let (_dir, root) = tempdir_root()?;
            let (_, plan) = single_direct_dependency(&root)?;
            let script = fake_pnpm_answering_exclusions(
                &root,
                indoc! {r#"
                      *" update "*)
                        exit 0 ;;
                "#},
                &format!("echo '{}'; exit 0", serde_json::to_string(scalar)?),
            )?;
            let tool = tool_with(&script)?;

            apply(&tool, &project(&root), plan).await?;

            let legs = legs(&root);
            let update = legs
                .iter()
                .find(|leg| leg.contains(" update "))
                .expect("the direct pin takes the joint update");
            let passed: Vec<&str> = update
                .split(' ')
                .filter_map(|arg| arg.strip_prefix("--config.minimumReleaseAgeExclude="))
                .collect();
            assert_eq!(passed.join(" "), expected, "{scalar}: {update}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn young_direct_and_transitive_targets_persist_exact_native_exclusions()
    -> eyre::Result<()> {
        for direct in [true, false] {
            assert_young_native_exclusions(direct, &[]).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn native_persistence_does_not_copy_effective_config_exclusions() -> eyre::Result<()> {
        for direct in [true, false] {
            assert_young_native_exclusions(direct, &["user-global-*", "external@9.0.0"]).await?;
        }
        Ok(())
    }

    /// The native exclusion rules before the run, and the ones persisted after it lands `foo@1.1.0`.
    struct NativeRules<'a> {
        before: &'a [&'a str],
        persisted: &'a [&'a str],
    }

    const CORP_RULES: NativeRules<'static> = NativeRules {
        before: &["@corp/*", "foo@0.9.0"],
        persisted: &["@corp/*", "foo@1.1.0"],
    };

    /// A persisted admission keeps the native rules in pnpm's order: it joins the package's first
    /// matching rule (here a negated one, ahead of which it gets its own), the shadowed stale rule
    /// goes, and every other rule keeps its place and spelling, a backslash included.
    #[tokio::test]
    async fn persisted_admissions_keep_the_native_rule_order() -> eyre::Result<()> {
        for direct in [true, false] {
            assert_young_native_exclusions_with(
                direct,
                &[],
                &NativeRules {
                    before: &["!other@1.0.0", "foo@0.9.0", "@corp/*", r"back\slash"],
                    persisted: &["foo@1.1.0", "!other@1.0.0", "@corp/*", r"back\slash"],
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn assert_young_native_exclusions(
        direct: bool,
        effective_exclusions: &[&str],
    ) -> eyre::Result<()> {
        assert_young_native_exclusions_with(direct, effective_exclusions, &CORP_RULES).await
    }

    async fn assert_young_native_exclusions_with(
        direct: bool,
        effective_exclusions: &[&str],
        rules: &NativeRules<'_>,
    ) -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = write_young_pin_lock(&root, direct)?;
        let native_before = write_native_rules(&root, rules.before)?;
        let script = fake_pnpm_with_exclusions(
            &root,
            indoc! {r#"
                      *"--frozen-lockfile"*)
                        grep -q 'foo@1.1.0' pnpm-workspace.yaml || exit 1
                        exit 0 ;;
                      *" update "*|*"install"*)
                        cp settled.yaml pnpm-lock.yaml; exit 0 ;;
                "#},
            effective_exclusions,
        )?;

        // The planner can admit a two-day-old security fix through its shorter window.
        // The adapter must retain that exact pin beneath pnpm's seven-day native gate.
        // The fixed January clock keeps the pin young even when this test runs months later.
        let cache = tempfile::tempdir()?;
        let now: jiff::Timestamp = "2026-01-18T00:00:00Z".parse()?;
        let tool = young_pin_tool(cache.path(), &script, now)?;
        let mut target = change("foo", "1.0.0", "1.1.0", &[("app", ".")]);
        target.direct = direct;
        if !direct {
            target.members.clear();
        }
        let plan = Plan {
            changes: vec![target],
            evaluated_at: Some(now),
            ..Plan::default()
        };
        let project = project(&root);
        let manifest_before = read(&root, "package.json")?;
        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;
        assert_eq!(applied_names(&report), vec!["foo"], "direct={direct}");
        let native = read(&root, "pnpm-workspace.yaml")?;
        assert!(!native.contains("overrides:"), "{native}");
        assert_eq!(
            NativeAge::read(&root.join("pnpm-workspace.yaml"))?
                .exclusions
                .into_vec(),
            rules.persisted,
            "direct={direct}: {native}"
        );
        for exclusion in effective_exclusions {
            assert!(
                read(&root, "legs.log")?.contains(exclusion),
                "effective config should remain available to repair CLI arguments"
            );
        }
        if direct {
            assert_joint_update_exempts_the_pin(&root, effective_exclusions)?;
        }
        assert_eq!(
            tool.verify_lock_current(&project).await?.status,
            cooldown_core::LockStatus::Current,
            "the persisted exception must prevent stale_lock"
        );

        // Candidate rejection uses the same journal after the native write, so the exact
        // allowance must disappear together with the rejected lock.
        mutation.journal().restore()?;
        assert_eq!(read(&root, "package.json")?, manifest_before);
        assert_eq!(read(&root, "pnpm-workspace.yaml")?, native_before);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        Ok(())
    }

    /// Writes the lock holding `foo@1.0.0`, directly declared or only reached through `parent`,
    /// and the lock the fake resolve settles on with `foo@1.1.0`; returns the first.
    fn write_young_pin_lock(root: &Utf8Path, direct: bool) -> eyre::Result<String> {
        let mut lock = workspace(
            root,
            &[Importer {
                path: ".",
                name: "app",
                deps: if direct {
                    vec![("foo", "^1.0.0", "1.0.0")]
                } else {
                    vec![("parent", "^1.0.0", "1.0.0")]
                },
            }],
        )?;
        if !direct {
            lock.push_str(&package_entry("foo", "1.0.0"));
            lock.push_str(indoc! {"

                    snapshots:
                      parent@1.0.0:
                        dependencies:
                          foo: 1.0.0
                "});
        }
        let settled = if direct {
            moved(&lock, ".", "foo", ("^1.0.0", "1.0.0"), ("^1.0.0", "1.1.0"))
        } else {
            lock.replace("foo@1.0.0", "foo@1.1.0")
                .replace("foo: 1.0.0", "foo: 1.1.0")
        };
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        std::fs::write(root.join("settled.yaml"), settled)?;
        Ok(lock)
    }

    /// Writes a native config with a seven-day window and `rules`, returning its text.
    fn write_native_rules(root: &Utf8Path, rules: &[&str]) -> eyre::Result<String> {
        let rules = rules
            .iter()
            .map(|rule| Ok(format!("  - {}\n", serde_json::to_string(rule)?)))
            .collect::<eyre::Result<String>>()?;
        let native =
            format!("packages: []\nminimumReleaseAge: 10080\nminimumReleaseAgeExclude:\n{rules}");
        std::fs::write(root.join("pnpm-workspace.yaml"), &native)?;
        Ok(native)
    }

    /// A pnpm tool driving `script` offline, whose registry cache publishes `foo@1.1.0` two days
    /// before `now`.
    fn young_pin_tool(
        cache: &std::path::Path,
        script: &Utf8Path,
        now: jiff::Timestamp,
    ) -> eyre::Result<NpmTool<Pnpm>> {
        let published = now.checked_sub(jiff::SignedDuration::from_hours(48))?;
        cooldown_registry::cache::write_entry(
            cache,
            &cooldown_registry::CacheEntry {
                url: "https://registry.npmjs.org/foo".to_string(),
                fetched_at: now.to_string(),
                etag: None,
                status: 200,
                body: serde_json::json!({
                    "versions": { "1.0.0": {}, "1.1.0": {} },
                    "time": { "1.0.0": "2020-01-01T00:00:00Z", "1.1.0": published.to_string() }
                })
                .to_string(),
                request_body: None,
            },
        )?;
        let mut tool = NpmTool::<Pnpm>::from_http(cooldown_registry::SharedHttp::new(
            cache,
            cooldown_registry::HttpOptions {
                offline: true,
                ..cooldown_registry::HttpOptions::default()
            },
        )?);
        tool.cmd = crate::nodecmd::NodeCmd::with_bin(script.as_str());
        Ok(tool)
    }

    /// The joint update exempts the young pin itself, beside the effective configured entries its
    /// command-line list replaces, so pnpm's strict gate never asks for an approval that
    /// `--no-save` cannot record.
    fn assert_joint_update_exempts_the_pin(
        root: &Utf8Path,
        effective_exclusions: &[&str],
    ) -> eyre::Result<()> {
        let legs = read(root, "legs.log")?;
        let update = legs
            .lines()
            .find(|leg| leg.contains(" update "))
            .expect("the direct pin takes the joint update");
        let mut expected: Vec<String> = effective_exclusions
            .iter()
            .map(|exclusion| format!("--config.minimumReleaseAgeExclude={exclusion}"))
            .collect();
        expected.push("--config.minimumReleaseAgeExclude=foo@1.1.0".to_string());
        for flag in &expected {
            assert!(
                update.contains(flag.as_str()),
                "{flag} missing from {update}"
            );
        }
        let pin_flags = update
            .matches("--config.minimumReleaseAgeExclude=foo@1.1.0")
            .count();
        assert_eq!(
            pin_flags,
            if effective_exclusions.is_empty() {
                2
            } else {
                1
            },
            "a lone exclusion is repeated so pnpm reads a list: {update}"
        );
        Ok(())
    }

    fn project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: Pnpm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
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
            .filter(|line| !line.starts_with("config get "))
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
        // pdf-view installs solid-js through `dependencies`, so the row has no structural cause
        // to name and must not invent the peer-only one.
        assert!(
            !detail.contains("peerDependencies"),
            "an install declaration earns no peer-only advice: {detail}"
        );
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

    /// `explain` reads every member's declaration of a name: the lock's specifier and resolved
    /// version where the importer records it, the manifest's range where only the manifest does,
    /// and the manifest fields in either case — so a peer-only importer and a manifest-only peer
    /// are both visible beside the installing one.
    #[tokio::test]
    async fn declarations_read_every_member_from_the_lock_and_its_manifest() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "apps/app",
                    name: "@x/app",
                    deps: vec![
                        ("solid-js", "^1.9.14", "1.9.14"),
                        ("lodash", "^4.17.20", "4.17.20"),
                    ],
                },
                Importer {
                    path: "packages/lib",
                    name: "@x/lib",
                    deps: vec![("solid-js", "^1.9.14", "1.9.14")],
                },
                Importer {
                    path: "packages/tool",
                    name: "@x/tool",
                    deps: vec![],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), lock)?;
        // The lock records lib's auto-installed peer under `dependencies:`; only the manifest says
        // it is a peer contract. tool declares the name in its manifest alone.
        std::fs::write(
            root.join("packages/lib/package.json"),
            r#"{ "name": "@x/lib", "peerDependencies": { "solid-js": "^1.9.14" } }"#,
        )?;
        std::fs::write(
            root.join("packages/tool/package.json"),
            r#"{ "name": "@x/tool", "devDependencies": { "solid-js": "^1.9.0" } }"#,
        )?;
        let tool = tool_with(&fake_pnpm(&root, "")?)?;

        let declarations = tool.declarations(&project(&root), "solid-js").await?;

        // Each row as `name path range resolved [fields]`, ascending by path.
        let rows: Vec<String> = declarations
            .iter()
            .map(|declaration| {
                format!(
                    "{} {} {} {} [{}]",
                    declaration.member.name,
                    declaration.member.path,
                    declaration.range.as_deref().unwrap_or("-"),
                    declaration.resolved.as_deref().unwrap_or("-"),
                    declaration.fields.join(", ")
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                "@x/app apps/app ^1.9.14 1.9.14 [dependencies]",
                "@x/lib packages/lib ^1.9.14 1.9.14 [peerDependencies]",
                "@x/tool packages/tool ^1.9.0 - [devDependencies]",
            ]
        );
        assert!(
            tool.declarations(&project(&root), "lodash").await?.len() == 1,
            "a name one importer declares yields one declaration"
        );
        Ok(())
    }

    /// A member declaring a name at several ranges shows each, with the fields spelling it and the
    /// version its lock entry resolves to, although the lock records only one of them.
    #[tokio::test]
    async fn declarations_show_every_range_a_member_declares() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        hidden_pin_fixture(&root, "20.14.5", "20.14.5")?;
        std::fs::write(
            root.join("apps/app/package.json"),
            indoc! {r#"
                {
                  "name": "app",
                  "dependencies": { "happy-dom": "^20.11.1" },
                  "devDependencies": { "happy-dom": "20.14.5" },
                  "optionalDependencies": { "happy-dom": "^20.11.1" }
                }
            "#},
        )?;
        let tool = tool_with(&fake_pnpm(&root, "")?)?;

        let declarations = tool.declarations(&project(&root), "happy-dom").await?;

        // Each row as `path range resolved [fields]`; the caret's two fields share one row.
        let rows: Vec<String> = declarations
            .iter()
            .map(|declaration| {
                format!(
                    "{} {} {} [{}]",
                    declaration.member.path,
                    declaration.range.as_deref().unwrap_or("-"),
                    declaration.resolved.as_deref().unwrap_or("-"),
                    declaration.fields.join(", ")
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                "apps/app ^20.11.1 20.14.5 [dependencies, optionalDependencies]",
                "apps/app 20.14.5 20.14.5 [devDependencies]",
            ]
        );
        Ok(())
    }

    /// The structural cause behind the luup partial landings: the importer left behind declares
    /// the package only in `peerDependencies`, which pnpm auto-installs and records in the lock
    /// but `pnpm update` has no install field to advance.
    /// The row names the importer and the remedy — a library adds a `devDependencies` entry, an
    /// application (`private: true`) moves the entry to `dependencies`.
    #[tokio::test]
    async fn a_partial_landing_names_a_peer_only_declaration_and_its_remedy() -> eyre::Result<()> {
        for (private, remedy) in [
            (
                false,
                "declare it in `devDependencies` as well to let it move",
            ),
            (true, "move it to `dependencies` to let it move"),
        ] {
            let (_dir, root) = tempdir_root()?;
            let tool = tool_with(&solid_js_fixture(&root)?)?;
            // The fixture's manifests install solid-js; only pdf-view's is rewritten to the
            // peer-only shape, so the advice must single it out.
            std::fs::write(
                root.join("packages/pdf-view/package.json"),
                format!(
                    r#"{{ "name": "@x/pdf-view", "private": {private}, "peerDependencies": {{ "solid-js": "^1.9.14" }} }}"#
                ),
            )?;
            let members = [
                ("@x/admin", "apps/admin"),
                ("@x/web", "apps/web"),
                ("@x/pdf-view", "packages/pdf-view"),
            ];
            let plan = Plan {
                changes: vec![change("solid-js", "1.9.14", "1.9.15", &members)],
                ..Plan::default()
            };

            let report = apply(&tool, &project(&root), plan).await?;

            let [held] = report.skipped.as_slice() else {
                panic!("the partial candidate is held: {:?}", report.skipped);
            };
            let detail = held
                .detail
                .as_deref()
                .expect("the rollback explains itself");
            // The rollback itself is unchanged; the sentence after it names the cause and the
            // one change that would let the importer move.
            assert!(
                detail.contains("rolled back rather than split solid-js"),
                "{detail}"
            );
            assert!(
                detail.contains(
                    "@x/pdf-view declares solid-js only in `peerDependencies`, which `pnpm update` cannot advance; "
                ),
                "{detail}"
            );
            assert!(detail.ends_with(remedy), "private = {private}: {detail}");
            assert!(
                !detail.contains("@x/admin declares") && !detail.contains("@x/web declares"),
                "the importers that took the target are not blamed: {detail}"
            );
        }
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
        assert_eq!(warning.kind, DiagnosticKind::DuplicateCopy);
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

    /// `app` at 9.9.1 and the excluded `legacy` at 9.8.0, both on `mongoose@^9.8.0`, with the lock
    /// a resolve that moves both importers' copies to 9.9.3 would write.
    fn mongoose_moved_in_both(root: &Utf8Path) -> eyre::Result<String> {
        let lock = mongoose_workspace(root, "9.9.1")?;
        let both = moved(
            &lock,
            "app",
            "mongoose",
            ("^9.8.0", "9.9.1"),
            ("^9.8.0", "9.9.3"),
        );
        Ok(moved(
            &both,
            "legacy",
            "mongoose",
            ("^9.8.0", "9.8.0"),
            ("^9.8.0", "9.9.3"),
        ))
    }

    /// The override repair re-resolves the whole workspace, so it moves the excluded importer's
    /// `^9.8.0` copy too.
    /// pnpm's native age gate refuses the windowed update, and the repair's resolution-only
    /// install under the temporary `mongoose@^9.0.0` override moves both range-compatible copies.
    /// The upgrade lands (an excluded subtree must not veto it) and the move is reported with the
    /// package attached, with the excluded manifest untouched and no override left behind.
    #[tokio::test]
    async fn an_excluded_importer_re_resolved_in_range_is_reported_not_rolled_back()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let both = mongoose_moved_in_both(&root)?;
        std::fs::write(root.join("both.yaml"), &both)?;
        // The update fails as pnpm's age gate fails a too-fresh starting lock.
        // Only the resolution-only install moves the lock, and only while the temporary override
        // is written; the settlement install must see the user's configuration restored.
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    echo '[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION] lock rejected' >&2; exit 1 ;;
                  *"--resolution-only"*)
                    grep -q 'mongoose@^9.0.0' pnpm-workspace.yaml || exit 1
                    cp both.yaml pnpm-lock.yaml; exit 0 ;;
                  *"install"*)
                    if grep -qs '^overrides:' pnpm-workspace.yaml; then exit 1; fi
                    exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("mongoose", "9.9.1", "9.9.3", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            ..Plan::default()
        };
        // A cutoff gives the update a native age window, the precondition of the age repair.
        let project = Project {
            exclude_newer: Some("2024-01-01T00:00:00Z".to_string()),
            ..project(&root)
        };

        let legacy_manifest = read(&root, "legacy/package.json")?;
        let report = apply(&tool, &project, plan).await?;

        // The refused update, then the repair's two installs, in that order
        let legs = legs(&root);
        let [update, repair, settle] = legs.as_slice() else {
            panic!("an update, then the repair's two installs: {legs:?}");
        };
        assert!(
            update.contains(" update ") && update.contains("--filter ./app"),
            "{update}"
        );
        assert!(!update.contains("legacy"), "{update}");
        assert!(repair.contains("--resolution-only"), "{repair}");
        assert!(
            settle.starts_with("install ") && !settle.contains("--resolution-only"),
            "{settle}"
        );

        // The upgrade lands with the excluded copy moved alongside it.
        assert_eq!(applied_names(&report), vec!["mongoose"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, both);
        let [warning] = report.warnings.as_slice() else {
            panic!(
                "the re-resolved excluded copy is reported once: {:?}",
                report.warnings
            );
        };
        // A move of an excluded importer's copy is neither a hold nor a duplicate: it has its own
        // kind, so the `outdated` summary's `held` count never has to reconcile with it.
        assert_eq!(warning.kind, DiagnosticKind::ExcludedMoved);
        assert_eq!(warning.package.as_deref(), Some("mongoose"));
        assert!(
            warning
                .message
                .contains("mongoose in legacy (9.8.0 → 9.9.3)"),
            "{}",
            warning.message
        );
        // `^9.8.0` provably admits 9.9.3, so the warning may say the move stayed in range.
        assert!(
            warning.message.contains("within the range"),
            "{}",
            warning.message
        );

        // The exclusion is honored where cooldown writes: no excluded manifest edit, and no
        // temporary override left in the workspace configuration.
        assert_eq!(read(&root, "legacy/package.json")?, legacy_manifest);
        assert!(
            !root.join("pnpm-workspace.yaml").exists(),
            "the temporary overrides must not persist"
        );
        Ok(())
    }

    /// An excluded importer whose unchanged declaration is a range cooldown cannot judge (an npm
    /// hyphen range) still has its move committed and reported, but the warning claims no range
    /// membership cooldown did not prove.
    #[tokio::test]
    async fn an_excluded_move_under_an_unjudged_range_is_reported_without_a_range_claim()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let hyphen = "9.8.0 - 9.8.5";
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("mongoose", "^9.8.0", "9.9.1")],
                },
                Importer {
                    path: "legacy",
                    name: "legacy",
                    deps: vec![("mongoose", hyphen, "9.8.0")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let settled = moved(
            &lock,
            "app",
            "mongoose",
            ("^9.8.0", "9.9.1"),
            ("^9.8.0", "9.9.3"),
        );
        let settled = moved(
            &settled,
            "legacy",
            "mongoose",
            (hyphen, "9.8.0"),
            (hyphen, "9.9.3"),
        );
        std::fs::write(root.join("settled.yaml"), &settled)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp settled.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("mongoose", "9.9.1", "9.9.3", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        // The acceptance policy is unchanged: an unprovable range is no proof of drift.
        assert_eq!(applied_names(&report), vec!["mongoose"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, settled);
        let [warning] = report.warnings.as_slice() else {
            panic!("the excluded move is reported once: {:?}", report.warnings);
        };
        assert_eq!(warning.kind, DiagnosticKind::ExcludedMoved);
        assert_eq!(warning.package.as_deref(), Some("mongoose"));
        assert!(
            warning
                .message
                .contains("mongoose in legacy (9.8.0 → 9.9.3)"),
            "{}",
            warning.message
        );
        // Only the wording changes: nothing proved 9.9.3 inside the hyphen range.
        assert!(
            !warning.message.contains("within the range"),
            "{}",
            warning.message
        );
        Ok(())
    }

    /// The control for the repair case above: a filtered update that succeeds moves only the
    /// importers it selects, so the excluded copy stays put, no repair runs, and nothing reports
    /// an excluded move.
    #[tokio::test]
    async fn a_filtered_update_leaves_an_excluded_importers_copy_in_place() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = mongoose_workspace(&root, "9.9.1")?;
        let app_only = moved(
            &lock,
            "app",
            "mongoose",
            ("^9.8.0", "9.9.1"),
            ("^9.8.0", "9.9.3"),
        );
        std::fs::write(root.join("app-only.yaml"), &app_only)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *" update "*)
                    cp app-only.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("mongoose", "9.9.1", "9.9.3", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            ..Plan::default()
        };
        let project = Project {
            exclude_newer: Some("2024-01-01T00:00:00Z".to_string()),
            ..project(&root)
        };

        let report = apply(&tool, &project, plan).await?;

        // One filtered update that never names the excluded importer, and no repair
        let legs = legs(&root);
        let [update] = legs.as_slice() else {
            panic!("only the filtered update runs: {legs:?}");
        };
        assert!(update.contains("--filter ./app"), "{update}");
        assert!(!update.contains("legacy"), "{update}");

        // The included importer lands; the excluded one keeps 9.8.0 and nothing is reported.
        assert_eq!(applied_names(&report), vec!["mongoose"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, app_only);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
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

    /// `~7.3.0` beside `^7.0.0`: a target the tilde range excludes within its own major line is
    /// held on its own line, with the row naming that range, why `--major` does not widen it, the
    /// target, and `--rewrite` as the way to converge — never calling the ranges "incompatible" —
    /// and pnpm is not asked to move it.
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
        assert_eq!(held.applied, []);
        let [skip] = held.skipped.as_slice() else {
            panic!("{:?}", held.skipped);
        };
        assert_eq!(skip.reason, SkipReason::MultiVersionHeld);
        let detail = skip.detail.as_deref().expect("detail");
        assert!(
            detail.contains("~7.3.0 excludes 7.4.0 within its own major line"),
            "the culprit range and why --major does not widen it: {detail}"
        );
        assert!(
            !detail.contains("^7.0.0"),
            "the caret admits the target and is not to blame: {detail}"
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

    /// Differently spelled carets on one resolved line, one per importer, beside a third range that
    /// already admits the target.
    fn caret_spellings_fixture(root: &Utf8Path) -> eyre::Result<String> {
        let lock = workspace(
            root,
            &[
                Importer {
                    path: "apps/frontend",
                    name: "frontend",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                },
                Importer {
                    path: "apps/legacy",
                    name: "legacy",
                    deps: vec![("happy-dom", "^20.9.0", "20.14.5")],
                },
                Importer {
                    path: "apps/open",
                    name: "open",
                    deps: vec![("happy-dom", ">=20.0.0", "20.14.5")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        Ok(lock)
    }

    const CARET_SPELLING_MEMBERS: [(&str, &str); 3] = [
        ("frontend", "apps/frontend"),
        ("legacy", "apps/legacy"),
        ("open", "apps/open"),
    ];

    /// A `--major` target that differently spelled carets exclude only by their implicit ceiling
    /// lands as it would under one shared range: one joint pin reaches every importer, each
    /// excluding caret widens to the target, and the range that admits it stays byte-identical.
    #[tokio::test]
    async fn differently_spelled_carets_take_a_cross_major_target_together() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = caret_spellings_fixture(&root)?;
        let open_manifest = read(&root, "apps/open/package.json")?;
        let landed = moved(
            &lock,
            "apps/frontend",
            "happy-dom",
            ("^20.11.1", "20.14.5"),
            ("^21.0.4", "21.0.4"),
        );
        let landed = moved(
            &landed,
            "apps/legacy",
            "happy-dom",
            ("^20.9.0", "20.14.5"),
            ("^21.0.4", "21.0.4"),
        );
        let landed = moved(
            &landed,
            "apps/open",
            "happy-dom",
            (">=20.0.0", "20.14.5"),
            (">=20.0.0", "21.0.4"),
        );
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"happy-dom@21.0.4"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "21.0.4",
                &CARET_SPELLING_MEMBERS,
            )],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["happy-dom"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        // Both carets widen to the target; the open range already admits it and is untouched.
        assert!(read(&root, "apps/frontend/package.json")?.contains(r#""happy-dom": "^21.0.4""#));
        assert!(read(&root, "apps/legacy/package.json")?.contains(r#""happy-dom": "^21.0.4""#));
        assert_eq!(read(&root, "apps/open/package.json")?, open_manifest);
        // One joint pin, filtered to every declaring importer.
        let legs = legs(&root);
        let [update] = legs.as_slice() else {
            panic!("one joint update: {legs:?}");
        };
        assert!(update.contains("happy-dom@21.0.4"), "{update}");
        for (_, path) in CARET_SPELLING_MEMBERS {
            assert!(update.contains(&format!("--filter ./{path}")), "{update}");
        }
        Ok(())
    }

    /// The lock records one specifier per importer, so an exact pin in a second install field is
    /// visible only in the manifest; the widen would rewrite it, so the name stays held and its
    /// row names the pin.
    #[tokio::test]
    async fn an_exact_pin_in_a_second_install_field_keeps_the_hold() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = caret_spellings_fixture(&root)?;
        let frontend = indoc! {r#"
            {
              "name": "frontend",
              "dependencies": { "happy-dom": "^20.11.1" },
              "devDependencies": { "happy-dom": "20.14.5" }
            }
        "#};
        std::fs::write(root.join("apps/frontend/package.json"), frontend)?;
        let legacy_manifest = read(&root, "apps/legacy/package.json")?;
        let script = fake_pnpm(&root, "")?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "21.0.4",
                &CARET_SPELLING_MEMBERS,
            )],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(report.applied, []);
        let [skip] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(skip.reason, SkipReason::PinHeld);
        let detail = skip.detail.as_deref().expect("detail");
        assert!(
            detail.contains("the exact pin 20.14.5 excludes 21.0.4"),
            "{detail}"
        );
        assert!(detail.contains("pass --rewrite"), "{detail}");
        assert_eq!(legs(&root), [] as [String; 0]);
        assert_eq!(read(&root, "apps/frontend/package.json")?, frontend);
        assert_eq!(read(&root, "apps/legacy/package.json")?, legacy_manifest);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        Ok(())
    }

    const HIDDEN_PIN_MEMBERS: [(&str, &str); 1] = [("app", "apps/app")];

    /// One importer declaring `happy-dom` at a caret in `dependencies` and at `second` in
    /// `devDependencies`, locked at `locked`, beside `lodash` on a caret.
    /// Like pnpm's own, the lock records only the `dependencies` specifier, so the lock alone sees
    /// one plain range a joint pin may land.
    fn hidden_pin_fixture(
        root: &Utf8Path,
        second: &str,
        locked: &'static str,
    ) -> eyre::Result<String> {
        let lock = workspace(
            root,
            &[Importer {
                path: "apps/app",
                name: "app",
                deps: vec![
                    ("happy-dom", "^20.11.1", locked),
                    ("lodash", "^4.17.20", "4.17.20"),
                ],
            }],
        )?;
        std::fs::write(
            root.join("apps/app/package.json"),
            formatdoc! {r#"
                {{
                  "name": "app",
                  "dependencies": {{ "happy-dom": "^20.11.1", "lodash": "^4.17.20" }},
                  "devDependencies": {{ "happy-dom": "{second}" }}
                }}
            "#},
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        Ok(lock)
    }

    /// An exact pin in a second install field holds a move the lock alone would pin jointly: the
    /// Auto widen would rewrite the pin, so the name is held, its row names the pin and the
    /// `--rewrite` remedy, and nothing is handed to pnpm.
    #[tokio::test]
    async fn a_hidden_exact_pin_holds_a_joint_pin() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = hidden_pin_fixture(&root, "20.14.5", "20.14.5")?;
        let manifest = read(&root, "apps/app/package.json")?;
        let tool = tool_with(&fake_pnpm(&root, "")?)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "20.14.6",
                &HIDDEN_PIN_MEMBERS,
            )],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(report.applied, []);
        let [skip] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(skip.reason, SkipReason::PinHeld);
        let detail = skip.detail.as_deref().expect("detail");
        assert!(
            detail.contains("the exact pin 20.14.5 excludes 20.14.6"),
            "{detail}"
        );
        assert!(detail.contains("pass --rewrite"), "{detail}");
        assert_eq!(legs(&root), [] as [String; 0]);
        assert_eq!(read(&root, "apps/app/package.json")?, manifest);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        Ok(())
    }

    /// A held hidden-pin candidate does not hold its batch: the unrelated sibling lands through the
    /// joint update, which carries no selector for the held name, and the held name's manifest
    /// declarations stay as written.
    #[tokio::test]
    async fn a_hidden_pin_hold_lets_an_unrelated_sibling_land() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = hidden_pin_fixture(&root, "20.14.5", "20.14.5")?;
        let manifest = read(&root, "apps/app/package.json")?;
        let landed = moved(
            &lock,
            "apps/app",
            "lodash",
            ("^4.17.20", "4.17.20"),
            ("^4.17.20", "4.17.21"),
        );
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"lodash@4.17.21"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![
                change("happy-dom", "20.14.5", "20.14.6", &HIDDEN_PIN_MEMBERS),
                change("lodash", "4.17.20", "4.17.21", &HIDDEN_PIN_MEMBERS),
            ],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["lodash"]);
        let [skip] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(skip.change.package.name, "happy-dom");
        assert_eq!(skip.reason, SkipReason::PinHeld);
        // The one joint update targets the sibling alone.
        let legs = legs(&root);
        let [update] = legs.as_slice() else {
            panic!("one joint update: {legs:?}");
        };
        assert!(update.contains("lodash@4.17.21"), "{update}");
        assert!(!update.contains("happy-dom"), "{update}");
        // lodash's caret already admits its target, so no declaration changes at all.
        assert_eq!(read(&root, "apps/app/package.json")?, manifest);
        Ok(())
    }

    /// A downgrade held by a hidden exact pin names `fix --downgrade-pinned`, never the `upgrade`
    /// flag `--rewrite`; the plan's permission to rewrite pins lands the same downgrade, rewriting
    /// the pin and keeping the caret that already admits the target.
    #[tokio::test]
    async fn a_hidden_pin_holds_a_downgrade_until_pins_may_be_rewritten() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = hidden_pin_fixture(&root, "20.14.6", "20.14.6")?;
        let manifest = read(&root, "apps/app/package.json")?;
        let landed = moved(
            &lock,
            "apps/app",
            "happy-dom",
            ("^20.11.1", "20.14.6"),
            ("^20.11.1", "20.14.5"),
        );
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"happy-dom@20.14.5"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = |rewrite_pins| Plan {
            changes: vec![Change {
                downgrade: true,
                ..change("happy-dom", "20.14.6", "20.14.5", &HIDDEN_PIN_MEMBERS)
            }],
            rewrite_pins,
            ..Plan::default()
        };

        // Plain `fix`: held, with the pin, the target, and the `fix` remedy named.
        let held = apply(&tool, &project(&root), plan(false)).await?;
        assert_eq!(held.applied, []);
        let [skip] = held.skipped.as_slice() else {
            panic!("{:?}", held.skipped);
        };
        assert_eq!(skip.reason, SkipReason::PinHeld);
        let detail = skip.detail.as_deref().expect("detail");
        assert!(
            detail.contains("the exact pin 20.14.6 excludes 20.14.5"),
            "{detail}"
        );
        assert!(
            detail.contains("rerun fix with --downgrade-pinned"),
            "{detail}"
        );
        assert!(!detail.contains("--rewrite"), "{detail}");
        assert_eq!(legs(&root), [] as [String; 0]);
        assert_eq!(read(&root, "apps/app/package.json")?, manifest);
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);

        // `fix --downgrade-pinned`: the pin is rewritten to the target and the downgrade lands.
        let landed = apply(&tool, &project(&root), plan(true)).await?;
        assert_eq!(applied_names(&landed), vec!["happy-dom"]);
        assert!(landed.skipped.is_empty(), "{:?}", landed.skipped);
        let after = read(&root, "apps/app/package.json")?;
        assert!(
            after.contains(r#""devDependencies": { "happy-dom": "20.14.5" }"#),
            "{after}"
        );
        assert!(after.contains(r#""happy-dom": "^20.11.1""#), "{after}");
        Ok(())
    }

    /// A pin the lock shows beside another importer's range splits the workspace, which the
    /// permission to rewrite pins does not reconcile: the downgrade stays held even with it, and
    /// the row asks for an edit instead of recommending `--downgrade-pinned`.
    #[tokio::test]
    async fn a_pin_split_across_importers_holds_a_downgrade_despite_the_permission()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "apps/pinned",
                    name: "pinned",
                    deps: vec![("happy-dom", "20.14.6", "20.14.6")],
                },
                Importer {
                    path: "apps/caret",
                    name: "caret",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.6")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let pinned = read(&root, "apps/pinned/package.json")?;
        let tool = tool_with(&fake_pnpm(&root, "")?)?;
        let plan = Plan {
            changes: vec![Change {
                downgrade: true,
                ..change(
                    "happy-dom",
                    "20.14.6",
                    "20.14.5",
                    &[("pinned", "apps/pinned"), ("caret", "apps/caret")],
                )
            }],
            rewrite_pins: true,
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(report.applied, []);
        let [skip] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(skip.reason, SkipReason::PinHeld);
        let detail = skip.detail.as_deref().expect("detail");
        assert!(
            detail.contains("the exact pin 20.14.6 excludes 20.14.5"),
            "{detail}"
        );
        assert!(detail.contains("edit the pin"), "{detail}");
        assert!(
            !detail.contains("rerun fix with --downgrade-pinned"),
            "the permission was granted and did not release it: {detail}"
        );
        assert!(!detail.contains("--rewrite"), "{detail}");
        assert_eq!(legs(&root), [] as [String; 0]);
        assert_eq!(read(&root, "apps/pinned/package.json")?, pinned);
        Ok(())
    }

    /// An exact declaration already equal to the target excludes nothing, so the joint pin lands
    /// and no declaration changes.
    #[tokio::test]
    async fn a_hidden_pin_at_the_target_lands() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = hidden_pin_fixture(&root, "20.14.6", "20.14.5")?;
        let manifest = read(&root, "apps/app/package.json")?;
        let landed = moved(
            &lock,
            "apps/app",
            "happy-dom",
            ("^20.11.1", "20.14.5"),
            ("^20.11.1", "20.14.6"),
        );
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"happy-dom@20.14.6"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "20.14.6",
                &HIDDEN_PIN_MEMBERS,
            )],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["happy-dom"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        assert_eq!(read(&root, "apps/app/package.json")?, manifest);
        Ok(())
    }

    /// `--rewrite` is the explicit authority to rewrite every declaration, the hidden pin
    /// included, so the move lands.
    #[tokio::test]
    async fn rewrite_lands_a_move_past_a_hidden_pin() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = hidden_pin_fixture(&root, "20.14.5", "20.14.5")?;
        let landed = moved(
            &lock,
            "apps/app",
            "happy-dom",
            ("^20.11.1", "20.14.5"),
            ("^20.14.6", "20.14.6"),
        );
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"happy-dom@20.14.6"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "20.14.6",
                &HIDDEN_PIN_MEMBERS,
            )],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["happy-dom"]);
        let after = read(&root, "apps/app/package.json")?;
        assert!(
            after.contains(r#""devDependencies": { "happy-dom": "20.14.6" }"#),
            "{after}"
        );
        Ok(())
    }

    /// node-semver's `v`-prefixed and spaced spellings of an exact pin, each with the prefix an
    /// authorized rewrite keeps: a bare spelling becomes the bare target, an `=` one `=target`.
    const PREFIXED_PIN_SPELLINGS: [(&str, &str); 3] = [("v", ""), ("=v", "="), ("= v", "=")];

    /// A prefixed hidden pin holds a joint upgrade like a bare one, with nothing handed to pnpm,
    /// even when the plan may rewrite pins for a downgrade; `--rewrite` lands the move and keeps
    /// the pin exact.
    #[tokio::test]
    async fn a_prefixed_hidden_pin_holds_a_joint_pin_until_rewrite() -> eyre::Result<()> {
        for (prefix, rewritten) in PREFIXED_PIN_SPELLINGS {
            let (_dir, root) = tempdir_root()?;
            let pin = format!("{prefix}20.14.5");
            let lock = hidden_pin_fixture(&root, &pin, "20.14.5")?;
            let manifest = read(&root, "apps/app/package.json")?;
            let landed = moved(
                &lock,
                "apps/app",
                "happy-dom",
                ("^20.11.1", "20.14.5"),
                ("^20.14.6", "20.14.6"),
            );
            std::fs::write(root.join("landed.yaml"), landed)?;
            let script = fake_pnpm(
                &root,
                indoc! {r#"
                      *"happy-dom@20.14.6"*)
                        cp landed.yaml pnpm-lock.yaml; exit 0 ;;
                "#},
            )?;
            let tool = tool_with(&script)?;
            let plan = |rewrite, rewrite_pins| Plan {
                changes: vec![change(
                    "happy-dom",
                    "20.14.5",
                    "20.14.6",
                    &HIDDEN_PIN_MEMBERS,
                )],
                rewrite,
                rewrite_pins,
                ..Plan::default()
            };

            for rewrite_pins in [false, true] {
                let report = apply(
                    &tool,
                    &project(&root),
                    plan(RewriteMode::Auto, rewrite_pins),
                )
                .await?;
                assert_eq!(report.applied, [], "{pin}");
                let [skip] = report.skipped.as_slice() else {
                    panic!("{pin}: {:?}", report.skipped);
                };
                assert_eq!(skip.reason, SkipReason::PinHeld, "{pin}");
                let detail = skip.detail.as_deref().expect("detail");
                assert!(
                    detail.contains(&format!("the exact pin {pin} excludes 20.14.6")),
                    "{detail}"
                );
                assert!(detail.contains("pass --rewrite"), "{detail}");
                assert_eq!(legs(&root), [] as [String; 0], "{pin}");
                assert_eq!(read(&root, "apps/app/package.json")?, manifest, "{pin}");
                assert_eq!(read(&root, "pnpm-lock.yaml")?, lock, "{pin}");
            }

            let report = apply(&tool, &project(&root), plan(RewriteMode::Always, false)).await?;
            assert_eq!(applied_names(&report), vec!["happy-dom"], "{pin}");
            let after = read(&root, "apps/app/package.json")?;
            assert!(
                after.contains(&format!(
                    r#""devDependencies": {{ "happy-dom": "{rewritten}20.14.6" }}"#
                )),
                "{after}"
            );
        }
        Ok(())
    }

    /// A prefixed hidden pin holds a downgrade until the plan may rewrite pins, which then lands
    /// it on an exact pin of the target and keeps the caret that already admits it.
    #[tokio::test]
    async fn a_prefixed_hidden_pin_holds_a_downgrade_until_pins_may_be_rewritten()
    -> eyre::Result<()> {
        for (prefix, rewritten) in PREFIXED_PIN_SPELLINGS {
            let (_dir, root) = tempdir_root()?;
            let pin = format!("{prefix}20.14.6");
            let lock = hidden_pin_fixture(&root, &pin, "20.14.6")?;
            let manifest = read(&root, "apps/app/package.json")?;
            let landed = moved(
                &lock,
                "apps/app",
                "happy-dom",
                ("^20.11.1", "20.14.6"),
                ("^20.11.1", "20.14.5"),
            );
            std::fs::write(root.join("landed.yaml"), landed)?;
            let script = fake_pnpm(
                &root,
                indoc! {r#"
                      *"happy-dom@20.14.5"*)
                        cp landed.yaml pnpm-lock.yaml; exit 0 ;;
                "#},
            )?;
            let tool = tool_with(&script)?;
            let plan = |rewrite_pins| Plan {
                changes: vec![Change {
                    downgrade: true,
                    ..change("happy-dom", "20.14.6", "20.14.5", &HIDDEN_PIN_MEMBERS)
                }],
                rewrite_pins,
                ..Plan::default()
            };

            let held = apply(&tool, &project(&root), plan(false)).await?;
            assert_eq!(held.applied, [], "{pin}");
            let [skip] = held.skipped.as_slice() else {
                panic!("{pin}: {:?}", held.skipped);
            };
            assert_eq!(skip.reason, SkipReason::PinHeld, "{pin}");
            let detail = skip.detail.as_deref().expect("detail");
            assert!(
                detail.contains(&format!("the exact pin {pin} excludes 20.14.5")),
                "{detail}"
            );
            assert!(
                detail.contains("rerun fix with --downgrade-pinned"),
                "{detail}"
            );
            assert_eq!(legs(&root), [] as [String; 0], "{pin}");
            assert_eq!(read(&root, "apps/app/package.json")?, manifest, "{pin}");
            assert_eq!(read(&root, "pnpm-lock.yaml")?, lock, "{pin}");

            let landed = apply(&tool, &project(&root), plan(true)).await?;
            assert_eq!(applied_names(&landed), vec!["happy-dom"], "{pin}");
            assert!(landed.skipped.is_empty(), "{:?}", landed.skipped);
            let after = read(&root, "apps/app/package.json")?;
            assert!(
                after.contains(&format!(
                    r#""devDependencies": {{ "happy-dom": "{rewritten}20.14.5" }}"#
                )),
                "{after}"
            );
            assert!(after.contains(r#""happy-dom": "^20.11.1""#), "{after}");
        }
        Ok(())
    }

    /// A prefixed pin the lock records beside another importer's caret splits the workspace like a
    /// bare one: the split holds the upgrade as a pin, naming the pin as written, and runs nothing.
    #[tokio::test]
    async fn a_prefixed_pin_split_across_importers_is_held_as_a_pin() -> eyre::Result<()> {
        for pin in ["v20.14.5", "=v20.14.5", "= v20.14.5"] {
            let (_dir, root) = tempdir_root()?;
            let lock = workspace(
                &root,
                &[
                    Importer {
                        path: "apps/pinned",
                        name: "pinned",
                        deps: vec![("happy-dom", pin, "20.14.5")],
                    },
                    Importer {
                        path: "apps/caret",
                        name: "caret",
                        deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                    },
                ],
            )?;
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            let pinned = read(&root, "apps/pinned/package.json")?;
            let tool = tool_with(&fake_pnpm(&root, "")?)?;
            let plan = Plan {
                changes: vec![change(
                    "happy-dom",
                    "20.14.5",
                    "20.14.6",
                    &[("pinned", "apps/pinned"), ("caret", "apps/caret")],
                )],
                ..Plan::default()
            };

            let report = apply(&tool, &project(&root), plan).await?;

            assert_eq!(report.applied, [], "{pin}");
            let [skip] = report.skipped.as_slice() else {
                panic!("{pin}: {:?}", report.skipped);
            };
            assert_eq!(skip.reason, SkipReason::PinHeld, "{pin}");
            let detail = skip.detail.as_deref().expect("detail");
            assert!(
                detail.contains(&format!("the exact pin {pin} excludes 20.14.6")),
                "{detail}"
            );
            assert_eq!(legs(&root), [] as [String; 0], "{pin}");
            assert_eq!(read(&root, "apps/pinned/package.json")?, pinned, "{pin}");
            assert_eq!(read(&root, "pnpm-lock.yaml")?, lock, "{pin}");
        }
        Ok(())
    }

    /// A prefixed pin the lock records for every declaring importer pins the dependency as a whole,
    /// so the planner holds it at its current version like a bare pin.
    #[tokio::test]
    async fn a_prefixed_lock_pin_is_reported_pinned() -> eyre::Result<()> {
        for pin in ["20.14.5", "v20.14.5", "=v20.14.5", "= v20.14.5"] {
            let (_dir, root) = tempdir_root()?;
            let lock = workspace(
                &root,
                &[Importer {
                    path: ".",
                    name: "app",
                    deps: vec![
                        ("happy-dom", pin, "20.14.5"),
                        ("lodash", "^4.17.20", "4.17.20"),
                    ],
                }],
            )?;
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            let tool = tool_with(&fake_pnpm(&root, "")?)?;

            let deps = tool.dependencies(&project(&root), DepScope::Direct).await?;

            let pinned: BTreeSet<&str> = deps
                .iter()
                .filter(|dep| dep.pinned)
                .map(|dep| dep.package.name.as_str())
                .collect();
            assert_eq!(pinned, BTreeSet::from(["happy-dom"]), "{pin}");
        }
        Ok(())
    }

    /// The hidden-pin importer of [`hidden_pin_fixture`] with a third, unrelated `nanoid`, and the
    /// locks a scripted pnpm answers with: `lodash`'s resolve also floats `happy-dom` within the
    /// recorded caret, past the `devDependencies` pin no planned change names, while `nanoid`'s
    /// resolve moves nothing else.
    fn sibling_moves_pin_fixture(root: &Utf8Path) -> eyre::Result<(String, Utf8PathBuf)> {
        let lock = workspace(
            root,
            &[Importer {
                path: "apps/app",
                name: "app",
                deps: vec![
                    ("happy-dom", "^20.11.1", "20.14.5"),
                    ("lodash", "^4.17.20", "4.17.20"),
                    ("nanoid", "^3.3.0", "3.3.7"),
                ],
            }],
        )?;
        std::fs::write(
            root.join("apps/app/package.json"),
            indoc! {r#"
                {
                  "name": "app",
                  "dependencies": { "happy-dom": "^20.11.1", "lodash": "^4.17.20", "nanoid": "^3.3.0" },
                  "devDependencies": { "happy-dom": "20.14.5" }
                }
            "#},
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let nanoid_only = moved(
            &lock,
            "apps/app",
            "nanoid",
            ("^3.3.0", "3.3.7"),
            ("^3.3.0", "3.3.8"),
        );
        let culprit = moved(
            &nanoid_only,
            "apps/app",
            "lodash",
            ("^4.17.20", "4.17.20"),
            ("^4.17.20", "4.17.21"),
        );
        let culprit = moved(
            &culprit,
            "apps/app",
            "happy-dom",
            ("^20.11.1", "20.14.5"),
            ("^20.11.1", "20.14.6"),
        );
        std::fs::write(root.join("nanoid-only.yaml"), nanoid_only)?;
        std::fs::write(root.join("culprit.yaml"), culprit)?;
        let script = fake_pnpm(
            root,
            indoc! {r#"
                  *"lodash@4.17.21"*)
                    cp culprit.yaml pnpm-lock.yaml; exit 0 ;;
                  *"nanoid@3.3.8"*)
                    cp nanoid-only.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        Ok((lock, script))
    }

    /// A sibling's resolve that floats a package past a manifest-only exact pin is refused, though
    /// no planned change names the pinned package and the lock's caret admits the move.
    /// The refusal names the owner, field, pin, and resulting version, and the recovery's safe
    /// subset, retried from the restored journal, lands with the pin kept.
    #[tokio::test]
    async fn a_sibling_moving_a_manifest_only_pin_is_refused_and_the_safe_subset_lands()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let (lock, script) = sibling_moves_pin_fixture(&root)?;
        let manifest = read(&root, "apps/app/package.json")?;
        let tool = tool_with(&script)?;
        let project = project(&root);
        let nanoid = change("nanoid", "3.3.7", "3.3.8", &HIDDEN_PIN_MEMBERS);
        let plan = Plan {
            changes: vec![
                change("lodash", "4.17.20", "4.17.21", &HIDDEN_PIN_MEMBERS),
                nanoid.clone(),
            ],
            ..Plan::default()
        };
        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;

        let error = tool
            .apply(&mutation)
            .await
            .expect_err("the joint resolve moved a protected pin");

        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("a resolver verdict the recovery isolates: {error:?}");
        };
        assert_eq!(
            detail,
            "the resolve moved an exact pin it was not asked to rewrite: happy-dom resolves to \
             20.14.6 for apps/app/package.json, whose devDependencies pins 20.14.5"
        );

        // The recovery restores the operation's journal and retries the subset without the culprit.
        mutation.journal().restore()?;
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        let report = tool.apply(&mutation.subset(vec![nanoid])?).await?;

        assert_eq!(applied_names(&report), vec!["nanoid"]);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
        let settled = read(&root, "pnpm-lock.yaml")?;
        assert!(settled.contains("happy-dom@20.14.5:"), "{settled}");
        assert!(!settled.contains("happy-dom@20.14.6"), "{settled}");
        assert!(settled.contains("nanoid@3.3.8:"), "{settled}");
        assert_eq!(read(&root, "apps/app/package.json")?, manifest);
        assert!(!root.join("pnpm-workspace.yaml").exists());
        Ok(())
    }

    /// How an untouched member's `package.json` is linked to its real file.
    #[derive(Clone, Copy)]
    enum ManifestLink {
        Symbolic,
        Hard,
    }

    /// A workspace whose managed `apps/app` lands `lodash` while the untouched `libs/linked`, whose
    /// manifest is a link, pins `happy-dom` in `devDependencies` beside the caret pnpm records.
    /// With `moves_pin`, lodash's resolve also floats the linked member's `happy-dom`.
    async fn linked_member_apply(
        root: &Utf8Path,
        link: ManifestLink,
        moves_pin: bool,
    ) -> eyre::Result<Result<ApplyReport>> {
        let lock = workspace(
            root,
            &[
                Importer {
                    path: "apps/app",
                    name: "app",
                    deps: vec![("lodash", "^4.17.20", "4.17.20")],
                },
                Importer {
                    path: "libs/linked",
                    name: "linked",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let manifest = root.join("libs/linked/package.json");
        let real = root.join("shared-linked-manifest.json");
        std::fs::write(
            &real,
            indoc! {r#"
                {
                  "name": "linked",
                  "dependencies": { "happy-dom": "^20.11.1" },
                  "devDependencies": { "happy-dom": "20.14.5" }
                }
            "#},
        )?;
        std::fs::remove_file(&manifest)?;
        match link {
            ManifestLink::Symbolic => std::os::unix::fs::symlink(&real, &manifest)?,
            ManifestLink::Hard => std::fs::hard_link(&real, &manifest)?,
        }
        let mut landed = moved(
            &lock,
            "apps/app",
            "lodash",
            ("^4.17.20", "4.17.20"),
            ("^4.17.20", "4.17.21"),
        );
        if moves_pin {
            landed = moved(
                &landed,
                "libs/linked",
                "happy-dom",
                ("^20.11.1", "20.14.5"),
                ("^20.11.1", "20.14.6"),
            );
        }
        std::fs::write(root.join("landed.yaml"), landed)?;
        let script = fake_pnpm(
            root,
            indoc! {r#"
                  *"lodash@4.17.21"*)
                    cp landed.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("lodash", "4.17.20", "4.17.21", &HIDDEN_PIN_MEMBERS)],
            ..Plan::default()
        };
        Ok(apply(&tool, &project(root), plan).await)
    }

    /// An untouched member whose manifest is a symbolic or hard link, which the journal's write
    /// restrictions refuse, does not block the mutation: its pins are read, not journaled, so an
    /// unrelated candidate lands, while a sibling's resolve moving the linked member's pin is
    /// still refused.
    #[tokio::test]
    async fn a_linked_untouched_manifest_neither_blocks_nor_loses_its_pins() -> eyre::Result<()> {
        for link in [ManifestLink::Symbolic, ManifestLink::Hard] {
            let (_dir, root) = tempdir_root()?;
            let report = linked_member_apply(&root, link, false).await??;
            assert_eq!(applied_names(&report), vec!["lodash"]);
            assert!(report.skipped.is_empty(), "{:?}", report.skipped);

            let (_moved_dir, moved_root) = tempdir_root()?;
            let refused = linked_member_apply(&moved_root, link, true).await?;
            let Err(CoreError::UnacceptableResolve(detail)) = refused else {
                panic!("the linked member's pin moved: {refused:?}");
            };
            assert!(
                detail.contains(
                    "happy-dom resolves to 20.14.6 for libs/linked/package.json, whose \
                     devDependencies pins 20.14.5"
                ),
                "{detail}"
            );
        }
        Ok(())
    }

    /// An excluded importer's manifest-only pin is protected too: pnpm moving the excluded copy
    /// within its recorded caret is otherwise an accepted re-resolve, but past the pin it is
    /// refused.
    #[tokio::test]
    async fn an_excluded_importers_manifest_only_pin_is_protected() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: ".",
                    name: "root",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                },
                Importer {
                    path: "apps/app",
                    name: "app",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                },
            ],
        )?;
        std::fs::write(
            root.join("package.json"),
            indoc! {r#"
                {
                  "name": "root",
                  "dependencies": { "happy-dom": "^20.11.1" },
                  "devDependencies": { "happy-dom": "20.14.5" }
                }
            "#},
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let both = moved(
            &lock,
            "apps/app",
            "happy-dom",
            ("^20.11.1", "20.14.5"),
            ("^20.11.1", "20.14.6"),
        );
        let both = moved(
            &both,
            ".",
            "happy-dom",
            ("^20.11.1", "20.14.5"),
            ("^20.11.1", "20.14.6"),
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
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "20.14.6",
                &HIDDEN_PIN_MEMBERS,
            )],
            excluded_members: vec![member("root", ".")],
            ..Plan::default()
        };

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("the excluded importer's pin moved");

        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("{error:?}");
        };
        assert!(
            detail.contains(
                "happy-dom resolves to 20.14.6 for package.json, whose devDependencies pins 20.14.5"
            ),
            "{detail}"
        );
        Ok(())
    }

    /// The exemption a pin rewrite earns lasts only while its change stays in the resolve: once the
    /// change is rolled back in an internal retry, the remainder's resolve may not keep its
    /// package past the restored pin.
    #[tokio::test]
    async fn a_change_removed_in_a_retry_loses_its_pin_exemption() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let solid = ("^1.9.14", "1.9.14");
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "apps/a",
                    name: "a",
                    deps: vec![
                        ("solid-js", solid.0, solid.1),
                        ("lodash", "^4.17.20", "4.17.20"),
                    ],
                },
                Importer {
                    path: "apps/b",
                    name: "b",
                    deps: vec![("solid-js", solid.0, solid.1)],
                },
            ],
        )?;
        std::fs::write(
            root.join("apps/a/package.json"),
            indoc! {r#"
                {
                  "name": "a",
                  "dependencies": { "solid-js": "^1.9.14", "lodash": "^4.17.20" },
                  "devDependencies": { "solid-js": "1.9.14" }
                }
            "#},
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let lodash = moved(
            &lock,
            "apps/a",
            "lodash",
            ("^4.17.20", "4.17.20"),
            ("^4.17.21", "4.17.21"),
        );
        // The first round lands solid-js in a alone, a partial landing the loop rolls back.
        let partial = moved(&lodash, "apps/a", "solid-js", solid, ("^1.9.15", "1.9.15"));
        // The retry without solid-js still floats it in both importers, within their carets.
        let floated = moved(&lodash, "apps/a", "solid-js", solid, ("^1.9.14", "1.9.15"));
        let floated = moved(&floated, "apps/b", "solid-js", solid, ("^1.9.14", "1.9.15"));
        std::fs::write(root.join("partial.yaml"), partial)?;
        std::fs::write(root.join("floated.yaml"), floated)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"solid-js@1.9.15"*)
                    cp partial.yaml pnpm-lock.yaml; exit 0 ;;
                  *"lodash@4.17.21"*)
                    cp floated.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![
                change(
                    "solid-js",
                    "1.9.14",
                    "1.9.15",
                    &[("a", "apps/a"), ("b", "apps/b")],
                ),
                change("lodash", "4.17.20", "4.17.21", &[("a", "apps/a")]),
            ],
            rewrite: RewriteMode::Always,
            ..Plan::default()
        };

        let error = apply(&tool, &project(&root), plan)
            .await
            .expect_err("the retry kept solid-js past a's restored pin");

        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("{error:?}");
        };
        assert!(
            detail.contains("solid-js resolves to 1.9.15 for apps/a/package.json, whose devDependencies pins 1.9.14"),
            "{detail}"
        );
        let legs = legs(&root);
        assert_eq!(
            legs.len(),
            2,
            "one round with solid-js, one without: {legs:?}"
        );
        Ok(())
    }

    /// The `happy-dom` downgrade a `fix --downgrade-pinned` authorizes, landed through the
    /// override repair after pnpm's targeted update floats the lock out of range, beside an
    /// unrelated `lodash` pin whose copy the repair moves when `moves_lodash` is set.
    async fn downgrade_through_repair(
        root: &Utf8Path,
        moves_lodash: bool,
    ) -> eyre::Result<Result<ApplyReport>> {
        let lock = workspace(
            root,
            &[Importer {
                path: "apps/app",
                name: "app",
                deps: vec![
                    ("happy-dom", "^20.11.1", "20.14.6"),
                    ("lodash", "^4.17.20", "4.17.20"),
                ],
            }],
        )?;
        std::fs::write(
            root.join("apps/app/package.json"),
            indoc! {r#"
                {
                  "name": "app",
                  "dependencies": { "happy-dom": "^20.11.1", "lodash": "^4.17.20" },
                  "devDependencies": { "happy-dom": "20.14.6", "lodash": "4.17.20" }
                }
            "#},
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let floated = moved(
            &lock,
            "apps/app",
            "happy-dom",
            ("^20.11.1", "20.14.6"),
            ("^20.11.1", "21.0.0"),
        );
        let mut repaired = moved(
            &lock,
            "apps/app",
            "happy-dom",
            ("^20.11.1", "20.14.6"),
            ("^20.11.1", "20.14.5"),
        );
        if moves_lodash {
            repaired = moved(
                &repaired,
                "apps/app",
                "lodash",
                ("^4.17.20", "4.17.20"),
                ("^4.17.20", "4.17.21"),
            );
        }
        std::fs::write(root.join("floated.yaml"), floated)?;
        std::fs::write(root.join("repaired.yaml"), repaired)?;
        let script = fake_pnpm(
            root,
            indoc! {r#"
                  *" update "*)
                    cp floated.yaml pnpm-lock.yaml; exit 0 ;;
                  *"install"*)
                    cp repaired.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![Change {
                downgrade: true,
                ..change("happy-dom", "20.14.6", "20.14.5", &HIDDEN_PIN_MEMBERS)
            }],
            rewrite_pins: true,
            ..Plan::default()
        };
        Ok(apply(&tool, &project(root), plan).await)
    }

    /// The pin rewrite `fix --downgrade-pinned` authorizes is recorded where the repair replays the
    /// widen, so the downgrade lands through the override repair, while the unrelated pin beside it
    /// stays protected there.
    #[tokio::test]
    async fn an_authorized_downgrade_lands_through_the_repair_beside_a_protected_pin()
    -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let landed = downgrade_through_repair(&root, false).await??;
        assert_eq!(applied_names(&landed), vec!["happy-dom"]);
        assert!(landed.skipped.is_empty(), "{:?}", landed.skipped);
        let after = read(&root, "apps/app/package.json")?;
        assert!(
            after.contains(r#""devDependencies": { "happy-dom": "20.14.5", "lodash": "4.17.20" }"#),
            "{after}"
        );

        let (_moved_dir, moved_root) = tempdir_root()?;
        let refused = downgrade_through_repair(&moved_root, true).await?;
        let Err(CoreError::UnacceptableResolve(detail)) = refused else {
            panic!("the repair moved the unrelated pin: {refused:?}");
        };
        assert_eq!(
            detail,
            "the resolve moved an exact pin it was not asked to rewrite: lodash resolves to \
             4.17.21 for apps/app/package.json, whose devDependencies pins 4.17.20"
        );
        Ok(())
    }

    /// The inconsistency a repair leaves behind in [`persistent_inconsistency`].
    #[derive(Clone, Copy)]
    enum Persistent {
        /// `dep` stays out of its caret range, and no pin moves.
        OutOfRange,
        /// `dep` stays out of its caret range, and the repair also moves the protected `lodash`
        /// pin.
        OutOfRangeMovingPin,
        /// `dep` lands at its target, but the importer's exact `pinned: 1.0.0` stays floated to
        /// `1.2.0`.
        /// The manifest declares the pin in `dependencies`, where it is protected, or in
        /// `peerDependencies`, where the pin guard does not judge it.
        FloatedExactPin { protected: bool },
    }

    /// A repair that leaves the lock inconsistent fails as a stale lock, unless the repair also
    /// moved a protected pin: that refusal is a resolver verdict the recovery can isolate, which a
    /// stale lock is not.
    async fn persistent_inconsistency(
        root: &Utf8Path,
        persistent: Persistent,
    ) -> eyre::Result<CoreError> {
        let lock = workspace(
            root,
            &[Importer {
                path: ".",
                name: "app",
                deps: vec![
                    ("dep", "^1.0.0", "1.0.0"),
                    ("lodash", "^4.17.20", "4.17.20"),
                    ("pinned", "1.0.0", "1.0.0"),
                ],
            }],
        )?;
        // The pin sits beside `dep` where the manifest protects it, and in `peerDependencies`,
        // which the pin guard does not judge, where nothing does.
        let (dependencies, peers) = match persistent {
            Persistent::FloatedExactPin { protected: false } => (
                r#""dep": "^1.0.0", "lodash": "^4.17.20""#,
                r#", "peerDependencies": { "pinned": "1.0.0" }"#,
            ),
            Persistent::OutOfRange
            | Persistent::OutOfRangeMovingPin
            | Persistent::FloatedExactPin { protected: true } => (
                r#""dep": "^1.0.0", "lodash": "^4.17.20", "pinned": "1.0.0""#,
                "",
            ),
        };
        std::fs::write(
            root.join("package.json"),
            formatdoc! {r#"
                {{
                  "name": "app",
                  "dependencies": {{ {dependencies} }},
                  "devDependencies": {{ "lodash": "4.17.20" }}{peers}
                }}
            "#},
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let out_of_range = moved(&lock, ".", "dep", ("^1.0.0", "1.0.0"), ("^1.0.0", "2.0.0"));
        let (floated, repaired) = match persistent {
            Persistent::OutOfRange => (out_of_range.clone(), out_of_range),
            Persistent::OutOfRangeMovingPin => {
                let repaired = moved(
                    &out_of_range,
                    ".",
                    "lodash",
                    ("^4.17.20", "4.17.20"),
                    ("^4.17.20", "4.17.21"),
                );
                (out_of_range, repaired)
            }
            Persistent::FloatedExactPin { .. } => {
                let landed = moved(&lock, ".", "dep", ("^1.0.0", "1.0.0"), ("^1.0.0", "1.1.0"));
                let floated = moved(
                    &landed,
                    ".",
                    "pinned",
                    ("1.0.0", "1.0.0"),
                    ("1.0.0", "1.2.0"),
                );
                (floated.clone(), floated)
            }
        };
        std::fs::write(root.join("floated.yaml"), floated)?;
        std::fs::write(root.join("repaired.yaml"), repaired)?;
        let script = fake_pnpm(
            root,
            indoc! {r#"
                  *" update "*)
                    cp floated.yaml pnpm-lock.yaml; exit 0 ;;
                  *"install"*)
                    cp repaired.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("dep", "1.0.0", "1.1.0", &[("app", ".")])],
            ..Plan::default()
        };
        match apply(&tool, &project(root), plan).await {
            Ok(report) => eyre::bail!("an inconsistent repair must fail: {report:?}"),
            Err(error) => Ok(error),
        }
    }

    /// The pin guard runs before the persistent-inconsistency fallback, so a repair that moved a
    /// protected pin is refused as such, while one that moved none still fails as a stale lock.
    #[tokio::test]
    async fn a_moved_pin_is_refused_before_the_stale_lock_fallback() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let error = persistent_inconsistency(&root, Persistent::OutOfRangeMovingPin).await?;
        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("the pin refusal wins: {error:?}");
        };
        assert!(
            detail.contains(
                "lodash resolves to 4.17.21 for package.json, whose devDependencies pins 4.17.20"
            ),
            "{detail}"
        );

        let (_stale_dir, stale_root) = tempdir_root()?;
        let error = persistent_inconsistency(&stale_root, Persistent::OutOfRange).await?;
        assert!(matches!(error, CoreError::StaleLock(_)), "{error:?}");
        Ok(())
    }

    /// An exact importer pin that the targeted `update` floats within its major and the repair
    /// leaves floated takes the same path.
    /// Where the manifest protects the pin, the pin refusal still wins over the stale lock.
    /// Where nothing protects it, the persistent mismatch fails as a stale lock instead of being
    /// committed.
    #[tokio::test]
    async fn a_persistently_floated_exact_pin_is_refused_or_stale() -> eyre::Result<()> {
        // Both runs reach the verdict through the repair: the targeted update floats the pin, and
        // the override engine's resolution-only install retries the same pins.
        let repaired = |root: &Utf8Path| {
            let legs = legs(root);
            assert!(
                legs.first().is_some_and(|leg| leg.contains(" update "))
                    && legs.iter().any(|leg| leg.contains("--resolution-only")),
                "the floated pin sends the round through the repair: {legs:?}"
            );
        };

        let (_dir, root) = tempdir_root()?;
        let error =
            persistent_inconsistency(&root, Persistent::FloatedExactPin { protected: true })
                .await?;
        let CoreError::UnacceptableResolve(detail) = error else {
            panic!("the pin refusal wins: {error:?}");
        };
        assert!(
            detail.contains(
                "pinned resolves to 1.2.0 for package.json, whose dependencies pins 1.0.0"
            ),
            "{detail}"
        );
        repaired(&root);

        let (_stale_dir, stale_root) = tempdir_root()?;
        let error = persistent_inconsistency(
            &stale_root,
            Persistent::FloatedExactPin { protected: false },
        )
        .await?;
        let CoreError::StaleLock(detail) = error else {
            panic!("an unprotected floated pin is a stale lock: {error:?}");
        };
        assert!(
            detail.contains("dependency pinned: version 1.2.0 does not satisfy range 1.0.0"),
            "{detail}"
        );
        repaired(&stale_root);
        Ok(())
    }

    /// A lock the run created has no baseline for the importer guards, yet its pins are still
    /// judged: a pinned dependency resolved off its pin is refused.
    #[test]
    fn the_pin_guard_runs_without_a_baseline_lock() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let settled = workspace(
            &root,
            &[Importer {
                path: ".",
                name: "app",
                deps: vec![("lodash", "^4.17.20", "4.17.21")],
            }],
        )?;
        std::fs::write(
            root.join("package.json"),
            r#"{ "name": "app", "devDependencies": { "lodash": "4.17.20" } }"#,
        )?;
        let baseline = MutationBaseline::read(&root, [Utf8PathBuf::from("package.json")])?;
        let protected = ProtectedPins::from_baseline::<Pnpm>(&baseline)?;
        let after_members = Pnpm::member_sources(&settled);
        let guard = |content: &str| {
            guard_settled_lock::<Pnpm>(
                &mut ApplyReport::default(),
                (None, content),
                (None, &after_members),
                &HashSet::new(),
                &SingleCopyPolicy::default(),
                PinGuard {
                    protected: &protected,
                    authorized: &AuthorizedPins::default(),
                    edits: EditStates::NONE,
                },
            )
        };

        let refused = guard(&settled);
        assert!(
            matches!(&refused, Err(CoreError::UnacceptableResolve(detail)) if detail.contains("lodash resolves to 4.17.21")),
            "{refused:?}"
        );
        let on_pin = settled.replace("4.17.21", "4.17.20");
        assert!(guard(&on_pin).is_ok());
        Ok(())
    }

    /// Only the declaring members the run manages can hold a move on a hidden pin: an excluded
    /// importer's pin is not the run's to judge, while the same pin in a managed member holds.
    #[test]
    fn an_excluded_importers_hidden_pin_does_not_hold() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: ".",
                    name: "root",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                },
                Importer {
                    path: "apps/app",
                    name: "app",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                },
            ],
        )?;
        std::fs::write(
            root.join("package.json"),
            indoc! {r#"
                {
                  "name": "root",
                  "dependencies": { "happy-dom": "^20.11.1" },
                  "devDependencies": { "happy-dom": "20.14.5" }
                }
            "#},
        )?;
        let managed = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "20.14.6",
                &HIDDEN_PIN_MEMBERS,
            )],
            excluded_members: vec![member("root", ".")],
            ..Plan::default()
        };
        let both = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "20.14.6",
                &[("root", "."), ("app", "apps/app")],
            )],
            ..Plan::default()
        };

        assert!(
            held_splits::<Pnpm>(Some(&root), Some(&lock), &managed)
                .names
                .is_empty()
        );
        let held = held_splits::<Pnpm>(Some(&root), Some(&lock), &both);
        assert_eq!(
            held.reasons.get("happy-dom"),
            Some(&SplitHold::ManifestPin {
                range: "20.14.5".to_string(),
            })
        );
        Ok(())
    }

    /// Released from the split hold, the joint pin is still verified per importer: when pnpm lands
    /// it in only some of them, the candidate is rolled back together with the widened carets.
    #[tokio::test]
    async fn a_partial_cross_major_landing_restores_the_widened_carets() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = caret_spellings_fixture(&root)?;
        let manifests = CARET_SPELLING_MEMBERS
            .iter()
            .map(|(_, path)| read(&root, &format!("{path}/package.json")))
            .collect::<eyre::Result<Vec<_>>>()?;
        let partial = moved(
            &lock,
            "apps/frontend",
            "happy-dom",
            ("^20.11.1", "20.14.5"),
            ("^21.0.4", "21.0.4"),
        );
        std::fs::write(root.join("partial.yaml"), partial)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"happy-dom@21.0.4"*)
                    cp partial.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "21.0.4",
                &CARET_SPELLING_MEMBERS,
            )],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(report.applied, []);
        let [skip] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(skip.reason, SkipReason::ResolverConflict);
        assert!(
            skip.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("rolled back"))
        );
        for ((_, path), before) in CARET_SPELLING_MEMBERS.iter().zip(&manifests) {
            assert_eq!(
                &read(&root, &format!("{path}/package.json"))?,
                before,
                "{path}"
            );
        }
        assert_eq!(read(&root, "pnpm-lock.yaml")?, lock);
        Ok(())
    }

    /// A declaring member whose manifest is gone cannot confirm the widen, so the name stays held
    /// and its row names the manifest.
    #[tokio::test]
    async fn a_missing_declaring_manifest_keeps_the_hold() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        caret_spellings_fixture(&root)?;
        std::fs::remove_file(root.join("apps/legacy/package.json"))?;
        let script = fake_pnpm(&root, "")?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "21.0.4",
                &CARET_SPELLING_MEMBERS,
            )],
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(report.applied, []);
        let [skip] = report.skipped.as_slice() else {
            panic!("{:?}", report.skipped);
        };
        assert_eq!(skip.reason, SkipReason::MultiVersionHeld);
        let detail = skip.detail.as_deref().expect("detail");
        assert!(
            detail.contains("apps/legacy/package.json could not be read"),
            "{detail}"
        );
        assert!(detail.contains("restore or repair it"), "{detail}");
        assert!(
            !detail.contains("--rewrite"),
            "no widen rewrites a missing manifest: {detail}"
        );
        assert_eq!(legs(&root), [] as [String; 0]);
        Ok(())
    }

    /// A transitive copy of a name has no declaring manifest, so it cannot confirm or veto the
    /// ceiling widen of the declared copy; in particular the root manifest it would fall back to
    /// may belong to an excluded importer whose exact pin is not the run's to judge.
    #[test]
    fn a_transitive_copy_does_not_veto_the_widen_of_the_declared_copy() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: ".",
                    name: "root",
                    deps: vec![("happy-dom", "20.14.5", "20.14.5")],
                },
                Importer {
                    path: "apps/frontend",
                    name: "frontend",
                    deps: vec![("happy-dom", "^20.11.1", "20.14.5")],
                },
                Importer {
                    path: "apps/legacy",
                    name: "legacy",
                    deps: vec![("happy-dom", "^20.9.0", "20.14.5")],
                },
            ],
        )?;
        let members = &CARET_SPELLING_MEMBERS[..2];
        let transitive = Change {
            from: Version::new("19.4.0"),
            direct: false,
            members: Vec::new(),
            ..change("happy-dom", "19.4.0", "21.0.4", &[])
        };
        let plan = Plan {
            changes: vec![
                change("happy-dom", "20.14.5", "21.0.4", members),
                transitive,
            ],
            excluded_members: vec![member("root", ".")],
            ..Plan::default()
        };

        assert!(
            held_splits::<Pnpm>(Some(&root), Some(&lock), &plan)
                .names
                .is_empty()
        );
        Ok(())
    }

    /// Without a project root to confirm the manifests, a ceiling widen stays held.
    #[test]
    fn a_ceiling_widen_without_manifests_stays_held() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = caret_spellings_fixture(&root)?;
        let plan = Plan {
            changes: vec![change(
                "happy-dom",
                "20.14.5",
                "21.0.4",
                &CARET_SPELLING_MEMBERS,
            )],
            ..Plan::default()
        };

        assert!(
            held_splits::<Pnpm>(None, Some(&lock), &plan)
                .names
                .contains("happy-dom")
        );
        assert!(
            held_splits::<Pnpm>(Some(&root), Some(&lock), &plan)
                .names
                .is_empty()
        );
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
        assert_eq!(held.applied, []);
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

        assert_eq!(report.applied, []);
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
        assert_eq!(legs(&root), [] as [String; 0]);
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
                held_splits::<Pnpm>(None, Some(&lock), &two_targets)
                    .names
                    .contains("@types/node"),
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
        assert!(
            held_splits::<Pnpm>(None, Some(&lock), &one_target)
                .names
                .is_empty()
        );
        // The multi-target hold needs no lock: without one the two pins would otherwise reach
        // pnpm together.
        let two_targets = Plan {
            changes: vec![
                change("@types/node", "22.19.20", "22.19.21", &low),
                change("@types/node", "25.9.2", "25.9.3", &high),
            ],
            ..Plan::default()
        };
        assert!(
            held_splits::<Pnpm>(None, None, &two_targets)
                .names
                .contains("@types/node")
        );
        assert!(
            held_splits::<Pnpm>(None, None, &one_target)
                .names
                .is_empty()
        );
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
        // the importer stays on 1.9.14; the snapshot edge is what names bar as the requirer.
        let mut settled = moved(
            &lock,
            "app",
            "bar",
            ("^1.0.0", "1.0.0"),
            ("^1.0.0", "1.1.0"),
        );
        settled.push_str(&package_entry("stateful", "1.8.0"));
        settled.push_str(indoc! {"

            snapshots:

              bar@1.1.0:
                dependencies:
                  stateful: 1.8.0
        "});
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
        // The copy was added, not held back, so the warning must not read as a `held` one — the
        // `outdated` summary's `held` count is about held rows and would otherwise never
        // reconcile with the warning block.
        assert_eq!(warning.kind, DiagnosticKind::DuplicateCopy);
        assert!(
            warning.message.contains("(1.9.14)") && warning.message.contains("1.8.0, 1.9.14"),
            "{}",
            warning.message
        );
        // The requirer is named from the settled lock's edges, so nobody greps `snapshots:`.
        assert!(
            warning
                .message
                .ends_with("a copy no importer declares, required at 1.8.0 by bar@1.1.0"),
            "{}",
            warning.message
        );
        Ok(())
    }

    /// The `single-copy` gate: the same second copy is refused instead of committed — the batch
    /// fails with the copy and its requirer named, so candidate isolation holds `bar` and restores
    /// the lock — under the configured list and under `--fail-on-new-duplicate` alike, each
    /// refusal naming the option responsible.
    #[tokio::test]
    async fn a_gated_name_gaining_a_second_copy_is_refused() -> eyre::Result<()> {
        for (policy, gate) in [
            (
                SingleCopyPolicy {
                    names: ["stateful".to_string()].into_iter().collect(),
                    every_name: false,
                },
                "`[tool.pnpm] single-copy`",
            ),
            (
                SingleCopyPolicy {
                    names: BTreeSet::default(),
                    every_name: true,
                },
                "`--fail-on-new-duplicate`",
            ),
        ] {
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
            let mut settled = moved(
                &lock,
                "app",
                "bar",
                ("^1.0.0", "1.0.0"),
                ("^1.0.0", "1.1.0"),
            );
            settled.push_str(&package_entry("stateful", "1.8.0"));
            settled.push_str(indoc! {"

                snapshots:

                  bar@1.1.0:
                    dependencies:
                      stateful: 1.8.0
            "});
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
                single_copy: policy,
                ..Plan::default()
            };

            let error = apply(&tool, &project(&root), plan)
                .await
                .expect_err("a gated second copy must not be committed");

            let CoreError::UnacceptableResolve(detail) = error else {
                panic!("a gated copy is a non-local rejection for candidate isolation: {error:?}");
            };
            assert!(
                detail.starts_with(&format!(
                    "the resolve added a second copy of stateful, which {gate} keeps single-copy: "
                )),
                "{detail}"
            );
            assert!(
                detail.ends_with("required at 1.8.0 by bar@1.1.0"),
                "the requirer is named on the refusal too: {detail}"
            );
        }
        Ok(())
    }

    /// The graph-level gate covers a name that was absent before the run and arrives at two
    /// copies at once — both undeclared, each attributed to its requirer — as a refusal under the
    /// listing and as a warning without it.
    #[tokio::test]
    async fn a_name_absent_before_the_run_and_at_two_copies_after_is_a_duplicate()
    -> eyre::Result<()> {
        for gated in [true, false] {
            let (_dir, root) = tempdir_root()?;
            let lock = workspace(
                &root,
                &[Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("bar", "^1.0.0", "1.0.0")],
                }],
            )?;
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            // `bar@1.1.0` requires `stateful@1.8.0` directly and `qux`, which requires
            // `stateful@2.0.0`: two copies of a name the graph did not hold before.
            let mut settled = moved(
                &lock,
                "app",
                "bar",
                ("^1.0.0", "1.0.0"),
                ("^1.0.0", "1.1.0"),
            );
            settled.push_str(&package_entry("stateful", "1.8.0"));
            settled.push_str(&package_entry("stateful", "2.0.0"));
            settled.push_str(&package_entry("qux", "1.0.0"));
            settled.push_str(indoc! {"

                snapshots:

                  bar@1.1.0:
                    dependencies:
                      qux: 1.0.0
                      stateful: 1.8.0

                  qux@1.0.0:
                    dependencies:
                      stateful: 2.0.0
            "});
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
                single_copy: SingleCopyPolicy {
                    names: gated.then(|| "stateful".to_string()).into_iter().collect(),
                    every_name: false,
                },
                ..Plan::default()
            };

            let outcome = apply(&tool, &project(&root), plan).await;

            let described = if gated {
                let Err(CoreError::UnacceptableResolve(detail)) = outcome else {
                    panic!("two new copies of a gated name are refused: {outcome:?}");
                };
                assert!(
                    detail.starts_with(
                        "the resolve added a second copy of stateful, which `[tool.pnpm] single-copy` keeps single-copy: "
                    ),
                    "{detail}"
                );
                detail
            } else {
                let report = outcome?;
                assert!(applied_names(&report).contains(&"bar"));
                let [warning] = report.warnings.as_slice() else {
                    panic!("the new copies are reported once: {:?}", report.warnings);
                };
                assert_eq!(warning.kind, DiagnosticKind::DuplicateCopy);
                warning.message.clone()
            };
            assert!(
                described.contains(
                    "stateful was absent from the graph before the run and is at several versions after it (1.8.0, 2.0.0)"
                ),
                "gated={gated}: {described}"
            );
            assert!(
                described
                    .ends_with("required at 1.8.0 by bar@1.1.0; required at 2.0.0 by qux@1.0.0"),
                "every copy is attributed: {described}"
            );
        }
        Ok(())
    }

    /// A listed name that was already at several copies before the run is nobody's landing to
    /// refuse — refusing would hold every upgrade forever — so it is reported instead, and the
    /// batch lands.
    #[tokio::test]
    async fn a_listed_name_already_at_several_copies_is_reported_not_refused() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let mut lock = workspace(
            &root,
            &[Importer {
                path: "app",
                name: "app",
                deps: vec![("bar", "^1.0.0", "1.0.0"), ("stateful", "^1.9.0", "1.9.14")],
            }],
        )?;
        // A transitive `stateful@1.8.0` beside the importer's 1.9.14, before anything runs.
        lock.push_str(&package_entry("stateful", "1.8.0"));
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        let settled = moved(
            &lock,
            "app",
            "bar",
            ("^1.0.0", "1.0.0"),
            ("^1.0.0", "1.1.0"),
        );
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
            single_copy: SingleCopyPolicy {
                names: ["stateful".to_string()].into_iter().collect(),
                every_name: false,
            },
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["bar"]);
        let [warning] = report.warnings.as_slice() else {
            panic!(
                "the unenforceable listing is reported once: {:?}",
                report.warnings
            );
        };
        assert_eq!(warning.kind, DiagnosticKind::DuplicateCopy);
        assert_eq!(warning.package.as_deref(), Some("stateful"));
        assert!(
            warning
                .message
                .contains("already resolved at several versions before the run (1.8.0, 1.9.14)"),
            "{}",
            warning.message
        );
        Ok(())
    }

    /// The importer guard judges a split from one importer version to several; a name no importer
    /// held before the run that arrives held at two versions is still a name at several copies
    /// where the graph had at most one, so the graph guard refuses it under the gate and reports
    /// it without — naming the importers, since no requirement edge binds an importer's own copy.
    #[tokio::test]
    async fn a_name_arriving_at_two_importer_held_copies_is_a_duplicate() -> eyre::Result<()> {
        for gated in [true, false] {
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
                        path: "lib",
                        name: "lib",
                        deps: vec![("bar", "^1.0.0", "1.0.0")],
                    },
                ],
            )?;
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            // The settled lock holds `stateful` in both importers at different versions, with no
            // copy anywhere before the run; `workspace` rewrites the manifests to match.
            let settled = workspace(
                &root,
                &[
                    Importer {
                        path: "app",
                        name: "app",
                        deps: vec![("bar", "^1.0.0", "1.1.0"), ("stateful", "^1.0.0", "1.0.0")],
                    },
                    Importer {
                        path: "lib",
                        name: "lib",
                        deps: vec![("bar", "^1.0.0", "1.1.0"), ("stateful", "^2.0.0", "2.0.0")],
                    },
                ],
            )?;
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
                changes: vec![change(
                    "bar",
                    "1.0.0",
                    "1.1.0",
                    &[("app", "app"), ("lib", "lib")],
                )],
                single_copy: SingleCopyPolicy {
                    names: BTreeSet::new(),
                    every_name: gated,
                },
                ..Plan::default()
            };

            let outcome = apply(&tool, &project(&root), plan).await;

            let described = if gated {
                let Err(CoreError::UnacceptableResolve(detail)) = outcome else {
                    panic!("two importer-held copies of a new name are refused: {outcome:?}");
                };
                assert!(
                    detail.starts_with(
                        "the resolve added a second copy of stateful, which `--fail-on-new-duplicate` keeps single-copy: "
                    ),
                    "{detail}"
                );
                detail
            } else {
                let report = outcome?;
                assert!(applied_names(&report).contains(&"bar"));
                let warning = report
                    .warnings
                    .iter()
                    .find(|warning| warning.kind == DiagnosticKind::DuplicateCopy)
                    .unwrap_or_else(|| {
                        panic!("the new copies are reported: {:?}", report.warnings)
                    });
                warning.message.clone()
            };
            // The copies are named with their holders rather than a requirer no edge binds.
            assert!(
                described.ends_with(
                    "stateful was absent from the graph before the run and is at several versions after it (1.0.0, 2.0.0): held by an importer: 1.0.0 in app, 2.0.0 in lib"
                ),
                "gated={gated}: {described}"
            );
        }
        Ok(())
    }

    /// A name already at several copies before the run gets its standing split reported, not
    /// refused; a copy the run adds beside that split is its doing all the same, so it is refused
    /// under either gate and reported with its requirer without one.
    #[tokio::test]
    async fn a_copy_added_beside_a_standing_split_is_a_duplicate() -> eyre::Result<()> {
        for gate in [
            Some("`[tool.pnpm] single-copy`"),
            Some("`--fail-on-new-duplicate`"),
            None,
        ] {
            let (_dir, root) = tempdir_root()?;
            let mut lock = workspace(
                &root,
                &[Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("bar", "^1.0.0", "1.0.0"), ("stateful", "^2.0.0", "2.0.0")],
                }],
            )?;
            // A transitive `stateful@1.0.0` beside the importer's 2.0.0, before anything runs.
            lock.push_str(&package_entry("stateful", "1.0.0"));
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            // `bar@1.1.0` requires `stateful@3.0.0`: a third copy the run adds.
            let mut settled = moved(
                &lock,
                "app",
                "bar",
                ("^1.0.0", "1.0.0"),
                ("^1.0.0", "1.1.0"),
            );
            settled.push_str(&package_entry("stateful", "3.0.0"));
            settled.push_str(indoc! {"

                snapshots:

                  bar@1.1.0:
                    dependencies:
                      stateful: 3.0.0
            "});
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
                single_copy: SingleCopyPolicy {
                    names: (gate == Some("`[tool.pnpm] single-copy`"))
                        .then(|| "stateful".to_string())
                        .into_iter()
                        .collect(),
                    every_name: gate == Some("`--fail-on-new-duplicate`"),
                },
                ..Plan::default()
            };

            let outcome = apply(&tool, &project(&root), plan).await;

            let described = if let Some(gate) = gate {
                let Err(CoreError::UnacceptableResolve(detail)) = outcome else {
                    panic!(
                        "a copy added beside a standing split is refused under {gate}: {outcome:?}"
                    );
                };
                assert!(
                    detail.starts_with(&format!(
                        "the resolve added another copy of stateful, which {gate} keeps single-copy: "
                    )),
                    "{detail}"
                );
                detail
            } else {
                let report = outcome?;
                assert!(applied_names(&report).contains(&"bar"));
                let [warning] = report.warnings.as_slice() else {
                    panic!(
                        "the added copy is reported once, not beside a standing-split note: {:?}",
                        report.warnings
                    );
                };
                assert_eq!(warning.kind, DiagnosticKind::DuplicateCopy);
                warning.message.clone()
            };
            assert!(
                described.ends_with(
                    "stateful was already at several versions before the run (1.0.0, 2.0.0) and gained another after it (3.0.0): a copy no importer declares, required at 3.0.0 by bar@1.1.0"
                ),
                "gate={gate:?}: {described}"
            );
        }
        Ok(())
    }

    /// A standing split gains a copy whichever side holds the new one: an importer moving onto a
    /// third version while a dependent keeps the one it left is the graph at three copies where
    /// it had two — refused under a gate and reported without one, with the holder named.
    #[tokio::test]
    async fn an_importer_moving_onto_a_third_copy_beside_a_standing_split_is_a_duplicate()
    -> eyre::Result<()> {
        for gated in [true, false] {
            let (_dir, root) = tempdir_root()?;
            let mut lock = workspace(
                &root,
                &[Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("stateful", "^2.0.0", "2.0.0")],
                }],
            )?;
            // A transitive `stateful@1.0.0` beside the importer's 2.0.0, before anything runs.
            lock.push_str(&package_entry("stateful", "1.0.0"));
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            // The importer moves to 2.5.0 within its range; a dependent still requires 2.0.0, so
            // that entry stays in the graph beside the new one.
            let mut settled = moved(
                &lock,
                "app",
                "stateful",
                ("^2.0.0", "2.0.0"),
                ("^2.0.0", "2.5.0"),
            );
            settled.push_str(&package_entry("stateful", "2.0.0"));
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
                changes: vec![change("stateful", "2.0.0", "2.5.0", &[("app", "app")])],
                single_copy: SingleCopyPolicy {
                    names: BTreeSet::new(),
                    every_name: gated,
                },
                ..Plan::default()
            };

            let outcome = apply(&tool, &project(&root), plan).await;

            let described = if gated {
                let Err(CoreError::UnacceptableResolve(detail)) = outcome else {
                    panic!("a third copy the importer moved onto is refused: {outcome:?}");
                };
                assert!(
                    detail.starts_with(
                        "the resolve added another copy of stateful, which `--fail-on-new-duplicate` keeps single-copy: "
                    ),
                    "{detail}"
                );
                detail
            } else {
                let report = outcome?;
                assert_eq!(applied_names(&report), vec!["stateful"]);
                let [warning] = report.warnings.as_slice() else {
                    panic!("the third copy is reported once: {:?}", report.warnings);
                };
                assert_eq!(warning.kind, DiagnosticKind::DuplicateCopy);
                warning.message.clone()
            };
            assert!(
                described.ends_with(
                    "stateful was already at several versions before the run (1.0.0, 2.0.0) and gained another after it (2.5.0): held by an importer: 2.5.0 in app"
                ),
                "gated={gated}: {described}"
            );
        }
        Ok(())
    }

    /// A copy that merely moves beside a standing split leaves the count alone: a dependent's
    /// requirement replacing the stray 1.0.0 with 4.0.0 is still two copies, so a listed name gets
    /// only its standing-split note, the flag says nothing, and the batch lands either way.
    #[tokio::test]
    async fn a_moved_copy_beside_a_standing_split_is_not_a_duplicate() -> eyre::Result<()> {
        for listed in [true, false] {
            let (_dir, root) = tempdir_root()?;
            let base = workspace(
                &root,
                &[Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("bar", "^1.0.0", "1.0.0"), ("stateful", "^2.0.0", "2.0.0")],
                }],
            )?;
            let mut lock = base.clone();
            lock.push_str(&package_entry("stateful", "1.0.0"));
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            // `bar@1.1.0` requires `stateful@4.0.0` where `bar@1.0.0` required 1.0.0: the stray
            // copy moves and nothing is added.
            let mut settled = moved(
                &base,
                "app",
                "bar",
                ("^1.0.0", "1.0.0"),
                ("^1.0.0", "1.1.0"),
            );
            settled.push_str(&package_entry("stateful", "4.0.0"));
            settled.push_str(indoc! {"

                snapshots:

                  bar@1.1.0:
                    dependencies:
                      stateful: 4.0.0
            "});
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
                single_copy: SingleCopyPolicy {
                    names: listed.then(|| "stateful".to_string()).into_iter().collect(),
                    every_name: !listed,
                },
                ..Plan::default()
            };

            let report = apply(&tool, &project(&root), plan).await?;

            // `stateful` shows as a collateral move, which is what it is.
            assert!(applied_names(&report).contains(&"bar"));
            if listed {
                let [note] = report.warnings.as_slice() else {
                    panic!("only the standing split is noted: {:?}", report.warnings);
                };
                assert!(
                    note.message.contains(
                        "already resolved at several versions before the run (1.0.0, 2.0.0)"
                    ),
                    "{}",
                    note.message
                );
            } else {
                assert!(report.warnings.is_empty(), "{:?}", report.warnings);
            }
        }
        Ok(())
    }

    /// An excluded importer left on a version the graph already had elsewhere adds no copy: two
    /// versions before and after, so a listed name is not refused — the deliberate split is
    /// reported as the exclusion asked, the standing split is noted, and the batch lands.
    #[tokio::test]
    async fn an_excluded_importer_left_on_an_existing_copy_is_not_refused() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let mut lock = mongoose_workspace(&root, "9.8.0")?;
        // A transitive `mongoose@9.9.3` already in the graph before the run.
        lock.push_str(&package_entry("mongoose", "9.9.3"));
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
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
                  *"--filter ./app --fail-if-no-match"*)
                    cp app-only.yaml pnpm-lock.yaml; exit 0 ;;
            "#},
        )?;
        let tool = tool_with(&script)?;
        let plan = Plan {
            changes: vec![change("mongoose", "9.8.0", "9.9.3", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            single_copy: SingleCopyPolicy {
                names: ["mongoose".to_string()].into_iter().collect(),
                every_name: false,
            },
            ..Plan::default()
        };

        let report = apply(&tool, &project(&root), plan).await?;

        assert_eq!(applied_names(&report), vec!["mongoose"]);
        let [split, standing] = report.warnings.as_slice() else {
            panic!(
                "the deliberate split and the standing split are each noted: {:?}",
                report.warnings
            );
        };
        assert!(
            split
                .message
                .contains("excluded from this run and were left in place"),
            "{}",
            split.message
        );
        assert!(
            standing
                .message
                .contains("already resolved at several versions before the run (9.8.0, 9.9.3)"),
            "{}",
            standing.message
        );
        Ok(())
    }

    /// A transitive third copy beside a deliberate excluded-importer split is still reported with
    /// its requirer: the split warning explains the importers' two copies, not a copy no importer
    /// declares.
    #[tokio::test]
    async fn a_third_copy_beside_a_deliberate_excluded_split_is_still_reported() -> eyre::Result<()>
    {
        let (_dir, root) = tempdir_root()?;
        let lock = mongoose_workspace(&root, "9.8.0")?;
        let mut app_only = moved(
            &lock,
            "app",
            "mongoose",
            ("^9.8.0", "9.8.0"),
            ("^9.8.0", "9.9.3"),
        );
        app_only.push_str(&package_entry("mongoose", "9.7.0"));
        app_only.push_str(&package_entry("legacy-tool", "1.0.0"));
        app_only.push_str(indoc! {"

            snapshots:

              legacy-tool@1.0.0:
                dependencies:
                  mongoose: 9.7.0
        "});
        std::fs::write(root.join("app-only.yaml"), app_only)?;
        let script = fake_pnpm(
            &root,
            indoc! {r#"
                  *"--filter ./app --fail-if-no-match"*)
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
        let [split, third] = report.warnings.as_slice() else {
            panic!(
                "the split and the third copy are each reported: {:?}",
                report.warnings
            );
        };
        assert!(
            split
                .message
                .contains("excluded from this run and were left in place"),
            "{}",
            split.message
        );
        assert_eq!(third.kind, DiagnosticKind::DuplicateCopy);
        assert!(
            third
                .message
                .ends_with("a copy no importer declares, required at 9.7.0 by legacy-tool@1.0.0"),
            "the third copy is attributed, not hidden behind the split: {}",
            third.message
        );
        Ok(())
    }

    /// A gated name is refused even when the second copy is the one an exclusion asked for: the
    /// excluded importer left on the old version is still a second copy in the lock the workspace
    /// installs. The way out is spelled for the gate that fired, and only promised when lifting
    /// the exclusion would converge the copies — not while another package still requires the
    /// left-behind version, which the refusal names instead.
    #[tokio::test]
    async fn a_gated_name_left_in_an_excluded_importer_is_refused() -> eyre::Result<()> {
        for case in [
            Case::Listed,
            Case::Flag,
            Case::Both,
            Case::ListedButRequired,
        ] {
            let (_dir, root) = tempdir_root()?;
            let lock = mongoose_workspace(&root, "9.8.0")?;
            let mut app_only = moved(
                &lock,
                "app",
                "mongoose",
                ("^9.8.0", "9.8.0"),
                ("^9.8.0", "9.9.3"),
            );
            if matches!(case, Case::ListedButRequired) {
                // `legacy-tool@1.0.0` requires the version the excluded importer was left on.
                app_only.push_str(&package_entry("legacy-tool", "1.0.0"));
                app_only.push_str(indoc! {"

                    snapshots:

                      legacy-tool@1.0.0:
                        dependencies:
                          mongoose: 9.8.0
                "});
            }
            std::fs::write(root.join("app-only.yaml"), app_only)?;
            let script = fake_pnpm(
                &root,
                indoc! {r#"
                      *"--filter ./app --fail-if-no-match"*)
                        cp app-only.yaml pnpm-lock.yaml; exit 0 ;;
                "#},
            )?;
            let tool = tool_with(&script)?;
            let (listed, flag) = case.gates();
            let plan = Plan {
                changes: vec![change("mongoose", "9.8.0", "9.9.3", &[("app", "app")])],
                excluded_members: vec![member("legacy", "legacy")],
                single_copy: SingleCopyPolicy {
                    names: listed.then(|| "mongoose".to_string()).into_iter().collect(),
                    every_name: flag,
                },
                ..Plan::default()
            };

            let error = apply(&tool, &project(&root), plan)
                .await
                .expect_err("a gated name split by an exclusion must not be committed");

            let CoreError::UnacceptableResolve(detail) = error else {
                panic!("a gated split is a non-local rejection for candidate isolation: {error:?}");
            };
            assert!(
                detail.contains("9.8.0 in legacy") && detail.contains("9.9.3 in app"),
                "the importers on each side: {detail}"
            );
            // The excluded importer never moves, so the refusal would recur forever without a
            // way out, spelled for every gate that fired; a copy another package requires gets
            // the ordinary refusal naming that requirer, since lifting the exclusion would not
            // converge it.
            let (prefix, suffix) = case.expectation();
            assert!(detail.starts_with(prefix), "{detail}");
            assert!(detail.ends_with(suffix), "{detail}");
        }
        Ok(())
    }

    /// The gate configurations and lock shapes of
    /// [`a_gated_name_left_in_an_excluded_importer_is_refused`].
    enum Case {
        Listed,
        Flag,
        Both,
        ListedButRequired,
    }

    impl Case {
        /// `(listed, flag)`.
        fn gates(&self) -> (bool, bool) {
            match self {
                Case::Listed | Case::ListedButRequired => (true, false),
                Case::Flag => (false, true),
                Case::Both => (true, true),
            }
        }

        /// The refusal's `(prefix, suffix)`.
        fn expectation(&self) -> (&'static str, &'static str) {
            match self {
                Case::Listed => (
                    "the resolve left an excluded importer on a second copy of mongoose, which `[tool.pnpm] single-copy` keeps single-copy: ",
                    "; drop mongoose from the listing, or bring legacy into the run (lift its exclusion) so the copies converge",
                ),
                Case::Flag => (
                    "the resolve left an excluded importer on a second copy of mongoose, which `--fail-on-new-duplicate` keeps single-copy: ",
                    "; run without `--fail-on-new-duplicate`, or bring legacy into the run (lift its exclusion) so the copies converge",
                ),
                Case::Both => (
                    "the resolve left an excluded importer on a second copy of mongoose, which `[tool.pnpm] single-copy` keeps single-copy: ",
                    "; drop mongoose from the listing and run without `--fail-on-new-duplicate`, or bring legacy into the run (lift its exclusion) so the copies converge",
                ),
                Case::ListedButRequired => (
                    "the resolve added a second copy of mongoose, which `[tool.pnpm] single-copy` keeps single-copy: ",
                    "; 9.8.0 in legacy is also required by legacy-tool@1.0.0",
                ),
            }
        }
    }

    /// The way out through the exclusion is promised only when lifting it would leave one copy:
    /// not beside a standing split whose other copy a dependent still requires, which gets the
    /// ordinary refusal with that requirer named.
    #[tokio::test]
    async fn a_left_behind_copy_beside_a_standing_split_promises_nothing() -> eyre::Result<()> {
        // A standing split: `legacy-a` at 1.0.0, `legacy-b` and `app` at 2.0.0; `app` moves to
        // 2.5.0 while `bar@1.0.0` keeps requiring 2.0.0.
        {
            let (_dir, root) = tempdir_root()?;
            let lock = workspace(
                &root,
                &[
                    Importer {
                        path: "app",
                        name: "app",
                        deps: vec![("stateful", "^2.0.0", "2.0.0"), ("bar", "^1.0.0", "1.0.0")],
                    },
                    Importer {
                        path: "legacy-a",
                        name: "legacy-a",
                        deps: vec![("stateful", "^1.0.0", "1.0.0")],
                    },
                    Importer {
                        path: "legacy-b",
                        name: "legacy-b",
                        deps: vec![("stateful", "^2.0.0", "2.0.0")],
                    },
                ],
            )?;
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            let mut settled = moved(
                &lock,
                "app",
                "stateful",
                ("^2.0.0", "2.0.0"),
                ("^2.0.0", "2.5.0"),
            );
            settled.push_str(&package_entry("stateful", "2.0.0"));
            settled.push_str(indoc! {"

                snapshots:

                  bar@1.0.0:
                    dependencies:
                      stateful: 2.0.0
            "});
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
                changes: vec![change("stateful", "2.0.0", "2.5.0", &[("app", "app")])],
                excluded_members: vec![
                    member("legacy-a", "legacy-a"),
                    member("legacy-b", "legacy-b"),
                ],
                single_copy: SingleCopyPolicy {
                    names: ["stateful".to_string()].into_iter().collect(),
                    every_name: false,
                },
                ..Plan::default()
            };

            let outcome = apply(&tool, &project(&root), plan).await;

            let Err(CoreError::UnacceptableResolve(detail)) = outcome else {
                panic!("a third copy beside a standing split is refused: {outcome:?}");
            };
            assert!(
                detail.starts_with(
                    "the resolve added another copy of stateful, which `[tool.pnpm] single-copy` keeps single-copy: "
                ),
                "{detail}"
            );
            // The required copy and the left-behind one are both named; nothing is promised.
            assert!(
                detail.ends_with(
                    "; 2.0.0 in legacy-b is also required by bar@1.0.0; 1.0.0 in legacy-a left behind by its exclusion"
                ) && !detail.contains("lift its exclusion"),
                "{detail}"
            );
        }
        Ok(())
    }

    /// Two included importers landing on two different new versions beside a left-behind excluded
    /// copy would still be at two copies with the exclusion lifted, so nothing is promised: the
    /// ordinary refusal names both new copies and the left-behind one.
    #[tokio::test]
    async fn two_included_importers_on_different_new_copies_promise_nothing() -> eyre::Result<()> {
        let (_dir, root) = tempdir_root()?;
        let lock = workspace(
            &root,
            &[
                Importer {
                    path: "app",
                    name: "app",
                    deps: vec![("stateful", "^2.0.0", "2.0.0")],
                },
                Importer {
                    path: "web",
                    name: "web",
                    deps: vec![("stateful", "^2.0.0", "2.0.0")],
                },
                Importer {
                    path: "legacy",
                    name: "legacy",
                    deps: vec![("stateful", "^1.0.0", "1.0.0")],
                },
            ],
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
        // `app` lands on the planned 2.5.0; pnpm re-resolves `web` to 2.7.0 within its range.
        let moved_app = moved(
            &lock,
            "app",
            "stateful",
            ("^2.0.0", "2.0.0"),
            ("^2.0.0", "2.5.0"),
        );
        let settled = moved(
            &moved_app,
            "web",
            "stateful",
            ("^2.0.0", "2.0.0"),
            ("^2.0.0", "2.7.0"),
        );
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
            changes: vec![change("stateful", "2.0.0", "2.5.0", &[("app", "app")])],
            excluded_members: vec![member("legacy", "legacy")],
            single_copy: SingleCopyPolicy {
                names: ["stateful".to_string()].into_iter().collect(),
                every_name: false,
            },
            ..Plan::default()
        };

        let outcome = apply(&tool, &project(&root), plan).await;

        let Err(CoreError::UnacceptableResolve(detail)) = outcome else {
            panic!("a third copy beside a standing split is refused: {outcome:?}");
        };
        assert!(
            detail.starts_with(
                "the resolve added another copy of stateful, which `[tool.pnpm] single-copy` keeps single-copy: "
            ),
            "{detail}"
        );
        assert!(
            detail.ends_with(
                "held by an importer: 2.5.0 in app, 2.7.0 in web; 1.0.0 in legacy left behind by its exclusion"
            ) && !detail.contains("lift its exclusion"),
            "{detail}"
        );
        Ok(())
    }

    /// An excluded importer that re-resolved onto the new copy within its range is not "left
    /// behind": only the excluded `legacy` declares `stateful`, upgrading `bar` in `app` lets
    /// pnpm move `legacy`'s copy from 1.0.0 to 1.1.0 (an accepted excluded move) while
    /// `bar@1.1.0` keeps requiring 1.0.0, so the refusal attributes both copies and promises
    /// nothing about the exclusion.
    #[tokio::test]
    async fn an_excluded_importer_on_the_new_copy_is_not_left_behind() -> eyre::Result<()> {
        {
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
                        deps: vec![("stateful", "^1.0.0", "1.0.0")],
                    },
                ],
            )?;
            std::fs::write(root.join("pnpm-lock.yaml"), &lock)?;
            let moved_bar = moved(
                &lock,
                "app",
                "bar",
                ("^1.0.0", "1.0.0"),
                ("^1.0.0", "1.1.0"),
            );
            let mut settled = moved(
                &moved_bar,
                "legacy",
                "stateful",
                ("^1.0.0", "1.0.0"),
                ("^1.0.0", "1.1.0"),
            );
            settled.push_str(&package_entry("stateful", "1.0.0"));
            settled.push_str(indoc! {"

                snapshots:

                  bar@1.1.0:
                    dependencies:
                      stateful: 1.0.0
            "});
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
                excluded_members: vec![member("legacy", "legacy")],
                single_copy: SingleCopyPolicy {
                    names: ["stateful".to_string()].into_iter().collect(),
                    every_name: false,
                },
                ..Plan::default()
            };

            let outcome = apply(&tool, &project(&root), plan).await;

            let Err(CoreError::UnacceptableResolve(detail)) = outcome else {
                panic!("a gated name at two copies is refused: {outcome:?}");
            };
            assert!(
                detail.starts_with(
                    "the resolve added a second copy of stateful, which `[tool.pnpm] single-copy` keeps single-copy: "
                ),
                "{detail}"
            );
            assert!(
                detail.ends_with(
                    "held by an importer: 1.1.0 in legacy; a copy no importer declares, required at 1.0.0 by bar@1.1.0"
                ) && !detail.contains("left behind"),
                "{detail}"
            );
        }
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
