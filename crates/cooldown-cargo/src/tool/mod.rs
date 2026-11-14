//! The Rust/Cargo [`Tool`](cooldown_core::Tool): detection, the
//! resolved graph via `cargo metadata`, classified releases from
//! the crates.io sparse index, and `cargo`-driven apply/build.
//!
//! The cooldown window is realized entirely in [`cooldown_core`]: cargo's own crates.io publish age
//! is read as native policy and kept out of the cargo commands this adapter runs (see the
//! `publish_age` module), because a resolver cutoff applied mid-apply can refuse a version the plan
//! still needs.
//! The crates.io sparse index supplies publish times, the core computes each crate's
//! newest-within-window target, and this adapter applies those targets as concrete
//! `cargo update --precise <version>` pins.
//!
//! Apply re-resolves the **whole** graph by issuing all planned pins as one logical unit.
//! Planned targets are seeded into the lock for one unlocked resolve; failed groups are bisected,
//! and only small rejected groups or targets the seed left short need individual precise commands.
//! A precise command Cargo refuses because a locked crate outside the plan blocks the target is
//! retried with that crate released, so an in-range target lands without a manifest edit.
//! Every lock-changing step is judged against the lock it started from and rolled back when it
//! moves a declared crate outside the plan or a planned crate off its course to its target.
//! Version reporting compares before/after `Cargo.lock` slots, while edge reporting audits moves
//! paired across stable dependent identities whose endpoints coexist in both snapshots.
//! The slot comparison reports planned and collateral version changes, while a planned candidate
//! that does not reach its target receives a held row.
//! Changed dependent identities and unpaired entries remain package-set changes rather than
//! attributable binding rows.
//! A converged graph re-applies to a byte-stable fixed point.

use crate::CARGO_ID;
use crate::cargocmd::{
    Cargo, ConflictParty, PackageKey, ResolvedGraph, conflict_parties,
    memoizable_resolver_rejection, seed_resolver_rejection,
};
use crate::command_memo::{CommandOutcomes, MemoizedCargo};
use crate::edges;
use crate::index::{CRATES_IO, CratesIoIndex};
use crate::locked_reads::{LockedReader, LockedReads};
use crate::lockfile::{
    CargoLock, DeclaredEndpoints, PlannedNodeMove, SlotKey, SourcedSlotKey, rewrite_planned_nodes,
};
use crate::manifest;
use crate::native::native_policy_layer;
use crate::publish_age::CargoEnvironment;
use crate::rejection_memo::{
    MemoKey, MemoMetadataMode, MemoOperation, MemoRejection, RejectionEffect, RejectionMemo,
    SplitHeal,
};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{RegistryVersionClassifier, build_registry_releases};
use cooldown_core::{
    ApplyAttempt, ApplyObserver, ApplyReport, Capabilities, Change, CoreError, DepScope,
    Dependency, Diagnostic, DiagnosticKind, EdgeNormalizationReport, EdgePolicy, EdgeRebind,
    FetchContext, GeneratedMembers, LockStatus, LockVerifyReport, MemberRef, MutationExecution,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, PreparedMutation, Project,
    ProjectMutationFile, ProjectMutationJournal, Release, ReleaseFetcher, ReleaseOrder,
    ReleaseQuality, ResolveInputs, ResolvedPolicy, Result, RewriteMode, SkipReason, Skipped,
    SyncReport, SyncScope, TargetFallback, ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport,
    Version, fs::RecoveryAuthority,
};
use cooldown_registry::SharedHttp;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod acceptance;
mod landing;

use acceptance::{ResolveBaseline, Violation};

/// The line cargo-hakari writes above the dependency tables it generates. Read for one purpose
/// only: to hint that an *undeclared* member looks like a workspace-hack. It never decides
/// anything — the marker is a convention, not a contract (forks of the tool exist), and a
/// supply-chain tool must not narrow its own scope from a heuristic.
const HAKARI_SECTION_MARKER: &str = "### BEGIN HAKARI SECTION";

/// How many stale projected crates a staleness notice names before eliding the rest.
const STALE_NODES_NAMED: usize = 6;

/// What one graph read establishes about a project's generated members
/// ([`Project::generated_members`]): the members themselves, as the manifests a lock move must
/// be followed into, and the notices the read gives rise to. Memoized per project root so the
/// notice pass, the mutation journal, and the apply need no graph read of their own.
#[derive(Debug, Default)]
struct GeneratedFacts {
    /// The declared generated members, resolved to their manifests.
    members: Vec<MemberRef>,
    /// The same members as lock identities (name, version, no source), so an edge observation
    /// can be matched to a generated member exactly rather than to any source-less namesake.
    identities: Vec<crate::lockfile::LockPackageId>,
    /// The authored members' lock identities — the roots a lock-text walk starts from, since a
    /// source-less lock block can also be an external path package.
    authored_identities: Vec<crate::lockfile::LockPackageId>,
    /// Advisory only (see [`ToolRead::manifest_notices`]).
    notices: Vec<Diagnostic>,
}

/// The Rust/Cargo implementation of the [`Tool`](cooldown_core::Tool) port.
///
/// Pairs the crates.io sparse-index client ([`CratesIoIndex`]) with a [`Cargo`]
/// CLI wrapper: the index supplies publish times and the release set, while
/// `cargo` resolves the dependency graph and applies precise version changes.
pub struct CargoTool {
    index: CratesIoIndex,
    cargo: Cargo,
    /// The [`GeneratedFacts`] per project root, established by the first graph read of that root
    /// (workspace membership cannot change within a run). A staged copy is a root of its own and
    /// establishes its own.
    generated: tokio::sync::Mutex<HashMap<Utf8PathBuf, Arc<GeneratedFacts>>>,
    /// The `(name, from, to)` moves a precise pin refused on their own during this run.
    ///
    /// Ordinary pin batches route it to its precise pin to avoid repeated bisection.
    /// Joint widen probes bypass this memory because their manifest context differs.
    refused_alone: std::sync::Mutex<HashSet<(String, String, String)>>,
    /// The other planned crates each refused change of the current resolve per project root was
    /// refused over: a conflict party withheld because it is planned, or a planned crate the step
    /// would have moved off its own plan.
    ///
    /// A plan without those crates may land the change, so its hold is not attested.
    refusal_constraints: std::sync::Mutex<HashMap<RefusalKey, BTreeSet<String>>>,
    /// The split completions of the current resolve per project root that did not land.
    ///
    /// A completion's outcome is a function of the project files it starts from, its coalition
    /// and the plan, and the plan is fixed for one resolve, so a later step meeting the same
    /// coalition in the same files skips the resolve.
    failed_completions: std::sync::Mutex<HashSet<CompletionKey>>,
    rejection_memo: RejectionMemo,
    rejection_memo_enabled: bool,
    /// Where earlier whole-graph resolves of the run ended, to warm-start later applies from.
    landings: landing::LandingMemo,
    /// The run's recent `cargo metadata --locked` reads, reused for identical native inputs.
    locked_reads: LockedReads,
    /// The run's recent lock-resolving commands in staged copies, replayed for identical native
    /// inputs.
    command_outcomes: CommandOutcomes,
}

impl CargoTool {
    /// Reads locked graphs, reusing an earlier read of the same native inputs in this run.
    fn locked_reader(&self) -> LockedReader<'_> {
        LockedReader {
            cargo: &self.cargo,
            memo: &self.rejection_memo,
            reads: &self.locked_reads,
        }
    }

    /// Runs the lock-resolving commands of an apply, replaying an earlier outcome of the same
    /// native inputs in this run.
    fn commands(&self) -> MemoizedCargo<'_> {
        MemoizedCargo {
            cargo: &self.cargo,
            memo: &self.rejection_memo,
            outcomes: &self.command_outcomes,
            reads: &self.locked_reads,
            enabled: self.rejection_memo_enabled,
        }
    }

    async fn metadata_with_observer(
        &self,
        project: &Project,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<ResolvedGraph> {
        if let Some(observer) = observer {
            observer.resolver_started(None);
        }
        self.commands().metadata(project).await
    }

    /// Creates an tool from an existing crates.io [`CratesIoIndex`] client.
    ///
    /// The [`Cargo`] CLI wrapper is constructed with its defaults (honoring the
    /// `COOLDOWN_CARGO` environment override).
    #[must_use]
    pub fn new(index: CratesIoIndex) -> Self {
        CargoTool {
            index,
            cargo: Cargo::new(),
            generated: tokio::sync::Mutex::new(HashMap::new()),
            refused_alone: std::sync::Mutex::new(HashSet::new()),
            refusal_constraints: std::sync::Mutex::new(HashMap::new()),
            failed_completions: std::sync::Mutex::new(HashSet::new()),
            rejection_memo: RejectionMemo::default(),
            rejection_memo_enabled: true,
            landings: landing::LandingMemo::default(),
            locked_reads: LockedReads::default(),
            command_outcomes: CommandOutcomes::default(),
        }
    }

    /// Enables or disables reuse of resolver rejections and Cargo command outcomes with identical
    /// resolution inputs.
    #[must_use]
    pub fn with_rejection_memo(mut self, enabled: bool) -> Self {
        self.rejection_memo_enabled = enabled;
        self
    }

    /// Registers `staged` as a staged copy of `original` whose topology lives in `tree`, a
    /// directory only cooldown writes.
    pub(crate) fn register_memo_origin(
        &self,
        staged: &Project,
        original: &Project,
        tree: &Utf8Path,
    ) {
        if !self.rejection_memo_enabled {
            return;
        }
        if let Err(err) = self
            .rejection_memo
            .register_origin(&staged.root, &original.root)
        {
            tracing::debug!(%err, "cargo rejection memo origin unavailable; bypassing memo");
        }
        self.command_outcomes.register_staged(&staged.root, tree);
    }

    /// The generated-member facts for `project`: the memoized ones, else established from a
    /// fresh locked graph read — which the notice pass never needs, since it follows the
    /// dependency read that memoized them, and a mutation needs at most once per staged copy.
    /// A project that names no generated member has no followers and, short of the memoized
    /// hint a dependency read may have left, no notices either, so it never spawns cargo for
    /// them: the common workspace pays nothing for the feature.
    async fn generated_facts(
        &self,
        project: &Project,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<Arc<GeneratedFacts>> {
        if let Some(facts) = self.generated.lock().await.get(&project.root) {
            return Ok(Arc::clone(facts));
        }
        if project.generated_members.names().is_empty() {
            return Ok(Arc::new(GeneratedFacts::default()));
        }
        if let Some(observer) = observer {
            observer.resolver_started(None);
        }
        let graph = self.locked_reader().graph(project).await?;
        self.remember_generated_facts(project, &graph).await
    }

    async fn pin_followers(&self, project: &Project) -> Result<Vec<MemberRef>> {
        if let Some(facts) = self.generated.lock().await.get(&project.root) {
            return Ok(facts.members.clone());
        }
        // An identical staged trial can hit the rejection memo before it has a locked graph.
        // Resolve follower names from workspace manifests rather than spawning Cargo for the key.
        match self.rejection_memo.followers(project) {
            Ok(followers) => Ok(followers),
            Err(err) => {
                tracing::debug!(%err, "cargo rejection memo follower discovery bypassed");
                Ok(self.generated_facts(project, None).await?.members.clone())
            }
        }
    }

    /// Establishes and memoizes the facts a graph read of `project` yields.
    async fn remember_generated_facts(
        &self,
        project: &Project,
        graph: &ResolvedGraph,
    ) -> Result<Arc<GeneratedFacts>> {
        let facts = Arc::new(generated_facts(project, graph));
        self.generated
            .lock()
            .await
            .insert(project.root.clone(), Arc::clone(&facts));
        Ok(facts)
    }

    /// Creates an tool backed by the shared HTTP layer, building the index for you.
    ///
    /// Convenience constructor equivalent to `CargoTool::new(CratesIoIndex::new(http))`.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        CargoTool::new(CratesIoIndex::new(http))
    }

    pub(crate) const fn cargo(&self) -> &Cargo {
        &self.cargo
    }
}

/// Derives the [`GeneratedFacts`] of `project` from one resolved `graph`.
///
/// With generated members declared, the notice is staleness: nodes only the generated members
/// reach are declared by a projection that nothing authored still needs, so the projection lags
/// the workspace and a regeneration would drop them. Without a declaration at all, the notice is
/// the hint: an authored member whose manifest carries the hakari marker looks like the shape the
/// declaration exists for, and its declarations are about to be proposed as upgrades. An explicit
/// empty declaration is the user saying every member is authored, and gets neither.
fn generated_facts(project: &Project, graph: &ResolvedGraph) -> GeneratedFacts {
    let members = graph.generated_member_refs();
    let mut notices = Vec::new();
    let stale = graph.nodes_reached_only_via_generated();
    if project.generated_members.is_declared() && !stale.is_empty() {
        let declared = members
            .iter()
            .map(|member| format!("`{}`", member.name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut named = stale
            .iter()
            .take(STALE_NODES_NAMED)
            .map(|(name, version)| format!("{name} {version}"))
            .collect::<Vec<_>>()
            .join(", ");
        if let Some(elided) = stale.len().checked_sub(STALE_NODES_NAMED)
            && elided > 0
        {
            use std::fmt::Write as _;
            let _ = write!(named, " and {elided} more");
        }
        let mut notice = Diagnostic::new(
            DiagnosticKind::StaleLock,
            format!(
                "generated member {declared} declares {} crate(s) no authored member reaches ({named}); the projection is stale relative to the workspace, so regenerate it (cargo-hakari: `cargo hakari generate`) and re-lock",
                stale.len()
            ),
        );
        if let [only] = members.as_slice() {
            notice = notice.with_path(manifest::member_manifest_rel(&only.path).as_str());
        }
        notices.push(notice);
    }
    if !project.generated_members.is_declared() {
        for member in hakari_marked_members(project, graph) {
            let declarations = graph.edges.get(&member.id).map_or(0, Vec::len);
            notices.push(
                Diagnostic::new(
                    DiagnosticKind::Config,
                    format!(
                        "member `{}` carries a `{HAKARI_SECTION_MARKER}` marker, so its manifest looks generated by cargo-hakari, yet cooldown treats its {declarations} dependency declarations as authored and proposes upgrades for them; declare it in `[tool.cargo] generated-members = [\"{}\"]` so they follow the lock instead, or set `generated-members = []` to keep them authored and silence this hint",
                        member.name, member.name
                    ),
                )
                .with_path(manifest::member_manifest_rel(&member.path).as_str()),
            );
        }
    }
    let identity_of = |root: &String| {
        graph.packages.get(root).map(|info| {
            crate::lockfile::LockPackageId::from_metadata(
                &info.name,
                &info.version,
                info.source.as_deref(),
            )
        })
    };
    let identities = graph
        .generated_roots
        .iter()
        .filter_map(identity_of)
        .collect();
    let authored_identities = graph
        .roots
        .iter()
        .filter(|root| !graph.generated_roots.contains(*root))
        .filter_map(identity_of)
        .collect();
    GeneratedFacts {
        members,
        identities,
        authored_identities,
        notices,
    }
}

/// A workspace member whose manifest carries the hakari marker, with its graph node id.
struct MarkedMember {
    id: String,
    name: String,
    path: String,
}

/// The authored members whose manifests carry the hakari marker, sorted by name. A manifest that
/// cannot be read contributes nothing: this is a hint, and a read failure is the graph read's to
/// report.
fn hakari_marked_members(project: &Project, graph: &ResolvedGraph) -> Vec<MarkedMember> {
    let mut marked: Vec<MarkedMember> = graph
        .roots
        .iter()
        .filter(|root| !graph.generated_roots.contains(*root))
        .filter_map(|root| graph.packages.get(root).map(|info| (root, info)))
        .filter(|(_, info)| {
            std::fs::read_to_string(project.root.join(manifest::member_manifest_rel(&info.path)))
                .is_ok_and(|manifest| manifest.contains(HAKARI_SECTION_MARKER))
        })
        .map(|(root, info)| MarkedMember {
            id: root.clone(),
            name: info.name.clone(),
            path: info.path.clone(),
        })
        .collect();
    marked.sort_by(|a, b| a.name.cmp(&b.name));
    marked
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// Classifies raw crates.io releases into ordered, deduped [`Release`]s relative to `current`.
///
/// Unparsable versions are dropped, the rest are sorted by [`version::compare`] and deduplicated,
/// then each is stamped with a [`ReleaseOrder`] token reflecting its rank (ascending). `current` is
/// the currently pinned version, used to compute each release's [`UpdateKind`]
/// via [`version::classify_kind`].
#[must_use]
pub fn build_releases(current: &str, raw: Vec<cooldown_core::RawRelease>) -> Vec<Release> {
    build_registry_releases(
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
    )
}

fn derive_locked_release(dep: &Dependency, releases: &[Release]) -> Option<Release> {
    let candidate = releases
        .iter()
        .find(|release| release.version == dep.current)?;
    Some(Release {
        version: dep.current.clone(),
        order: ReleaseOrder(Vec::new()),
        major: version::major_key(dep.current.as_str()),
        major_number: version::major_number(dep.current.as_str()),
        kind_from_current: None,
        beyond_declared_bound: false,
        beyond_latest_tag: false,
        published_at: candidate.published_at,
        yanked: false,
        quality: dep.current_quality,
    })
}

#[async_trait]
impl ToolRead for CargoTool {
    fn version_command(&self) -> Option<cooldown_core::trace::ToolVersionCommand> {
        Some(cooldown_core::trace::ToolVersionCommand {
            program: self.cargo.program().to_string(),
            arguments: vec!["--version".into()],
        })
    }

    fn evidence(&self, project: &Project) -> Result<cooldown_core::trace::Evidence> {
        use cooldown_core::trace::GraphCoverage;
        let mut evidence = cooldown_adapter_util::evidence::native(
            project,
            "Cargo.lock",
            GraphCoverage::Resolved,
            &["rust-toolchain", "rust-toolchain.toml"],
        );
        cooldown_adapter_util::evidence::ancestors(
            &mut evidence,
            &project.root,
            &[".cargo/config", ".cargo/config.toml"],
        );
        let candidates =
            cooldown_adapter_util::evidence::candidates(&project.root, &["Cargo.toml"])?;
        let directories: Vec<_> = candidates
            .into_iter()
            .filter_map(|path| path.parent().map(Utf8Path::to_owned))
            .collect();
        for member in crate::ownership::workspace_member_directories(&project.root, &directories) {
            evidence.inputs.push(cooldown_core::trace::EvidenceInput {
                path: member.join("Cargo.toml"),
                role: cooldown_core::trace::InputRole::Manifest,
            });
        }
        let (local, ambient) = crate::staging::cargo_config_dirs(&project.root)?;
        let config_dirs = local.union(&ambient).cloned().collect();
        for path in crate::staging::cargo_config_paths(&config_dirs)? {
            evidence.inputs.push(cooldown_core::trace::EvidenceInput {
                path,
                role: cooldown_core::trace::InputRole::Configuration,
            });
        }
        evidence.limitations.push("build scripts, external sources and Cargo config includes may read further inputs; actual native metadata and validated mutation inputs are recorded when consumed".into());
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &[
                "CARGO_HOME",
                "CARGO_BUILD_TARGET",
                "RUSTFLAGS",
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTUP_TOOLCHAIN",
                "CARGO_UNSTABLE_LOCKFILE_PATH",
            ],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        CARGO_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: false,
            artifact_granular: false,
            advisory_ecosystem: Some("crates.io"),
            honors_generated_members: true,
        }
    }

    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        crate::PROJECT_MARKER
    }

    fn nested_ownership(
        &self,
        primary: &[Utf8PathBuf],
        nested: &[Utf8PathBuf],
    ) -> Vec<cooldown_core::NestedOwnership> {
        crate::ownership::nested_ownership(primary, nested)
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        crate::staging::reject_custom_lockfile(&project.root)?;
        edges::enforce::ensure_no_pending(project)?;
        let graph = self.locked_reader().graph(project).await?;
        self.remember_generated_facts(project, &graph).await?;
        let reaching = graph.reaching_members_by_node();
        let mut deps = Vec::new();
        for (id, info) in &graph.packages {
            if graph.roots.contains(id) || !info.is_crates_io() {
                continue; // skip workspace members and non-crates.io sources
            }
            let direct = graph.is_direct(id);
            if scope == DepScope::Direct && !direct {
                continue;
            }
            // The demanded minimum across this node's active non-root requirers, read from the
            // resolved graph. A re-resolve picks the newest version each requirer's range admits, so a
            // transitive node can float far above this floor; recording it lets `fix`/reconcile mature
            // a too-fresh node back down to the newest release still at or above the floor.
            let graph_floor = graph
                .graph_floor(&info.name, &info.version)
                .map(|floor| Version::new(floor.to_string()));
            // A workspace member's own exact pin is `pinned` (held, with a repin target shown); the
            // ceiling is reserved for *transitive* caps a requirer imposes that the project cannot
            // repin away, so it is only set when the node is not already pinned.
            let pinned = graph.is_exact_pinned(&info.name, &info.version);
            deps.push(Dependency {
                package: PackageId::new(CARGO_ID, info.name.clone(), Some(CRATES_IO.to_string())),
                // Non-crates.io sources are skipped above, so every dependency here is provably
                // the crate OSV's `crates.io` ecosystem describes.
                advisory_identity: Some(info.name.clone()),
                current: Version::new(info.version.clone()),
                current_quality: classify_quality(&info.version),
                direct,
                artifacts: Vec::new(),
                graph_floor,
                graph_ceiling: (!pinned && graph.is_graph_capped(&info.name, &info.version))
                    .then(|| Version::new(info.version.clone())),
                declared_bound: graph
                    .declared_bound(&info.name, &info.version)
                    .map(str::to_string),
                // Direct deps are attributed to their declarers; a transitive dep is attributed to
                // the members that reach it through the graph (rendered as "via …").
                members: if direct {
                    graph.direct_members(id)
                } else {
                    reaching.get(id.as_str()).cloned().unwrap_or_default()
                },
                pinned,
                hold_edges: graph.node_hold_edges(&info.name, &info.version).to_vec(),
            });
        }
        Ok(deps)
    }

    async fn manifest_notices(&self, project: &Project) -> Result<Vec<Diagnostic>> {
        Ok(self.generated_facts(project, None).await?.notices.clone())
    }

    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>> {
        native_policy_layer(
            &project.root,
            &project.manifest,
            &CargoEnvironment::current(),
        )
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<LockVerifyReport> {
        crate::staging::reject_custom_lockfile(&project.root)?;
        edges::enforce::ensure_no_pending(project)?;
        match self.locked_reader().graph(project).await {
            Ok(_) => Ok(LockVerifyReport {
                status: LockStatus::Current,
                detail: "Cargo.lock is current".to_string(),
            }),
            // The probe reports what cargo refused, verbatim: the detail already names the project
            // — a repository can hold several Cargo locks (a parked nested workspace, standalone
            // fuzz targets) and `check` prints the detail without the path — and already carries
            // the remedy for the refusal cargo actually raised, so deriving a second one here
            // could only disagree with it.
            Err(CoreError::StaleLock(detail)) => Ok(LockVerifyReport {
                status: LockStatus::Stale,
                detail,
            }),
            Err(e) => Err(e),
        }
    }

    async fn lock_inputs(&self, project: &Project) -> Result<Vec<Utf8PathBuf>> {
        edges::enforce::ensure_no_pending(project)?;
        crate::staging::lock_input_manifests(&self.cargo, project).await
    }
}

#[async_trait]
impl ReleaseFetcher for CargoTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.index.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
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
            .index
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

    fn locked_release_from_candidates(
        &self,
        dep: &Dependency,
        releases: &[Release],
    ) -> Option<Release> {
        derive_locked_release(dep, releases)
    }
}

/// Selects the currently resolved node that represents a planned change without changing the
/// change's immutable baseline `from` version.
///
/// The original crates.io node wins while it exists. Once another planned pin has moved it, the
/// retry may address a unique off-target crates.io node in the target slot. Multiple candidates are
/// ambiguous Cargo version lines, so the adapter leaves them untouched for the final held
/// classification instead of risking a pin against the wrong line.
fn current_selector(lock: &CargoLock, change: &Change) -> Option<String> {
    if change.package.registry.as_deref() != Some(CRATES_IO) {
        return None;
    }
    let slots = lock.crates_io_locked_slots();
    let source_key = (
        change.package.name.clone(),
        version::major_key(change.from.as_str()).0,
    );
    let target_key = (
        change.package.name.clone(),
        version::major_key(change.to.as_str()).0,
    );
    let source_versions = slots.get(&source_key);
    if source_versions.is_some_and(|versions| versions.contains(change.from.as_str())) {
        return Some(change.from.to_string());
    }

    let target_versions = slots.get(&target_key);
    if target_versions.is_some_and(|versions| versions.contains(change.to.as_str())) {
        return None;
    }
    if let Some(version) = target_versions
        .filter(|versions| versions.len() == 1)
        .and_then(|versions| versions.first())
    {
        return Some(version.clone());
    }

    if source_key != target_key {
        return source_versions
            .filter(|versions| versions.len() == 1)
            .and_then(|versions| versions.first())
            .cloned();
    }
    None
}

/// The paired version-slot changes that `applied` does not already report, as sorted collateral
/// rows.
///
/// Exclusion is by exact registry plus `(name, from, to)` move, not by planned package name: a
/// planned candidate the resolve *held* can still have been floated off its baseline by a sibling
/// pin, and that real movement must surface beside its held skip row instead of being silently
/// dropped. Slots are compared per registry source, so a crates.io crate and an
/// alternate-registry crate sharing a name and major can neither pair with each other nor borrow
/// each other's registry label.
fn collateral_changes(
    before: &BTreeMap<SourcedSlotKey, String>,
    after: &BTreeMap<SourcedSlotKey, String>,
    applied: &[Change],
    declared: &DeclaredSides,
) -> Vec<Change> {
    let reported: BTreeSet<(&str, &str, &str, &str)> = applied
        .iter()
        .map(|change| {
            (
                change.package.registry.as_deref().unwrap_or(CRATES_IO),
                change.package.name.as_str(),
                change.from.as_str(),
                change.to.as_str(),
            )
        })
        .collect();
    // Group each side's version lines per source and name. Pairing only within a slot line would
    // lose every cross-line float — a companion crate dragged to its dependent's next major
    // (cranelift 0.133 → 0.134 beside wasmtime 46 → 47) is exactly the collateral this report
    // exists to surface, and 0.x lines make even minor companions cross-line.
    let mut before_by_name: BTreeMap<(&String, &String), Vec<&String>> = BTreeMap::new();
    for ((source, name, _), version) in before {
        before_by_name
            .entry((source, name))
            .or_default()
            .push(version);
    }
    let mut after_by_name: BTreeMap<(&String, &String), Vec<&String>> = BTreeMap::new();
    for ((source, name, _), version) in after {
        after_by_name
            .entry((source, name))
            .or_default()
            .push(version);
    }
    let mut changes: Vec<Change> = Vec::new();
    for ((source, name), before_versions) in &before_by_name {
        let empty = Vec::new();
        let after_versions = after_by_name.get(&(*source, *name)).unwrap_or(&empty);
        // Identical versions on both sides are unmoved lines; only the residuals moved.
        let mut from_residual: Vec<&String> = before_versions
            .iter()
            .filter(|version| !after_versions.contains(version))
            .copied()
            .collect();
        let mut to_residual: Vec<&String> = after_versions
            .iter()
            .filter(|version| !before_versions.contains(version))
            .copied()
            .collect();
        from_residual.sort_by(|a, b| version::compare(a, b));
        to_residual.sort_by(|a, b| version::compare(a, b));
        let pairs: Vec<(&String, &String)> = if from_residual.len() == to_residual.len() {
            // Equal residual counts: each line moved somewhere; rank order pairs each old line
            // with its successor (coexisting lines keep their relative order across a re-lock).
            from_residual.into_iter().zip(to_residual).collect()
        } else {
            // A line appeared or vanished (a fork or a dropped duplicate): rank pairing would
            // misattribute, so fall back to same-line pairing and leave the structural change to
            // the lock diff.
            from_residual
                .into_iter()
                .filter_map(|from| {
                    let line = version::major_key(from).0;
                    to_residual
                        .iter()
                        .find(|to| version::major_key(to).0 == line)
                        .map(|to| (from, *to))
                })
                .collect()
        };
        // The compact label the slot's lock source resolves to (`crates.io` for the default
        // registry), computed once per group and matched against the applied rows' registry.
        let registry = cooldown_core::redact::source_label(source);
        for (from, to) in pairs {
            if version::compare(from, to).is_ne()
                && !reported.contains(&(
                    registry.as_str(),
                    name.as_str(),
                    from.as_str(),
                    to.as_str(),
                ))
            {
                let direct = declared.declares_move(source, name, from, to);
                changes.push(collateral_change(&registry, name, from, to, direct));
            }
        }
    }
    changes.sort_by(|a, b| {
        a.package
            .name
            .cmp(&b.package.name)
            .then_with(|| a.package.registry.cmp(&b.package.registry))
            .then_with(|| a.from.as_str().cmp(b.from.as_str()))
    });
    changes
}

/// A paired version-slot change that no planned row reports, labeled with the registry its lock
/// source resolves to.
///
/// The whole-graph re-resolve can force collateral movement, such as pushing a transitive backward
/// for consistency or maturing a crate down during `fix`.
/// `direct` says whether an authored package declares the node that moved (see [`DeclaredSides`]).
fn collateral_change(registry: &str, name: &str, from: &str, to: &str, direct: bool) -> Change {
    let downgrade = version::compare(to, from).is_lt();
    Change {
        package: PackageId::new(CARGO_ID, name.to_string(), Some(registry.to_string())),
        from: Version::new(from.to_string()),
        to: Version::new(to.to_string()),
        // A collateral move is consistency churn, not a planned bump; its kind is informational
        // only and `Minor` is the neutral label the renderer shows.
        kind: cooldown_core::UpdateKind::Minor,
        downgrade,
        direct,
        members: Vec::new(),
    }
}

/// The nodes the authored packages declare before and after an apply, which decide whether a
/// collateral row is direct.
#[derive(Default)]
struct DeclaredSides {
    before: DeclaredEndpoints,
    after: DeclaredEndpoints,
}

impl DeclaredSides {
    /// The declarations of the lock an apply started from, when it was readable, and of the lock
    /// it left.
    fn of(before: Option<&CargoLock>, after: &CargoLock, generated: &GeneratedMembers) -> Self {
        DeclaredSides {
            before: before
                .map(|lock| lock.declared_endpoints(generated))
                .unwrap_or_default(),
            after: after.declared_endpoints(generated),
        }
    }

    /// Whether the `name` move from `from` to `to` in the lock `source` leaves or reaches a node an
    /// authored package declares.
    fn declares_move(&self, source: &str, name: &str, from: &str, to: &str) -> bool {
        self.before.declares(name, from, source) || self.after.declares(name, to, source)
    }
}

/// Pin-rejection diagnostics keyed by planned-change identity. Keying by name alone would let
/// coexisting-major plans for one crate (cargo forks distinct majors side by side) overwrite each
/// other's diagnostics and attach one target's rejection to the other's held row.
type PinRejections = BTreeMap<(String, String, String), String>;

#[derive(Default)]
struct MemoStats {
    hits: usize,
    misses: usize,
}

impl MemoStats {
    fn include(&mut self, other: &Self) {
        self.hits += other.hits;
        self.misses += other.misses;
    }
}

#[derive(Default)]
struct PinBatchStats {
    resolves: usize,
    /// Target visits in seeded resolves, including failed attempts.
    seeded: usize,
    /// Exact target landings kept from seeded resolves.
    landed: usize,
    bisected: usize,
    per_crate: usize,
    memo: MemoStats,
    resolver_rejected: bool,
    memo_disqualified: bool,
    rejection_effects: Vec<RejectionEffect>,
    /// The splits the batch's refusals stand on (see [`MemoRejection::heals`]).
    heals: Vec<SplitHeal>,
}

impl PinBatchStats {
    fn record_rejection(&mut self, resolver_rejection: bool) {
        self.resolver_rejected |= resolver_rejection;
        self.memo_disqualified |= !resolver_rejection;
    }

    fn memoizable_rejection(&self) -> bool {
        self.resolver_rejected && !self.memo_disqualified
    }

    fn record_effects(&mut self, rejection: &MemoRejection, rejections: &mut PinRejections) {
        rejection.apply(rejections);
        self.rejection_effects
            .extend(rejection.effects.iter().cloned());
        self.heals.extend(rejection.heals.iter().cloned());
    }
}

struct SeedGroup<'a> {
    moves: &'a [PlannedNodeMove],
    changes: &'a [&'a Change],
    /// The whole plan, which the seeded resolve's [acceptance rule](acceptance) is judged against.
    plan: &'a [Change],
}

struct MemoAttempt<'a> {
    project: &'a Project,
    /// Every change of the plan, which decides whether a refusal guarded by a split still stands
    /// (see [`MemoRejection::heals`]).
    plan: &'a [Change],
    operation: MemoOperation,
    metadata_mode: MemoMetadataMode,
    changes: &'a [&'a Change],
    followers: &'a [MemberRef],
    journal: &'a ProjectMutationJournal,
}

impl MemoAttempt<'_> {
    fn validate(&self) -> Result<()> {
        self.journal.validate_project(&self.project.root)?;
        cooldown_core::interrupt::ensure_not_requested("checking cargo rejection memo")
    }

    fn key(&self, memo: &RejectionMemo) -> Result<MemoKey> {
        memo.key(
            self.project,
            self.operation,
            self.changes,
            self.followers,
            self.metadata_mode,
        )
    }
}

/// The outcome of a whole-graph resolve.
struct WholeGraphResolve {
    rejections: PinRejections,
    /// The state the resolve started and ended in, when it warm-started and landed nothing more.
    unmoved: Option<UnmovedResolve>,
}

/// A warm-started resolve that ended where it started: the key of its inputs, its starting lock,
/// and the files its landing rewrote.
struct UnmovedResolve {
    key: MemoKey,
    lock: CargoLock,
    files: Vec<(Utf8PathBuf, String)>,
}

/// What a whole-graph resolve started from: the key of its inputs and the write set's contents.
struct LandingStart {
    key: MemoKey,
    lock: CargoLock,
    files: Vec<(Utf8PathBuf, Option<Vec<u8>>)>,
}

struct SeededResolve {
    before: CargoLock,
    after: CargoLock,
    snapshot: Vec<(Utf8PathBuf, Option<String>)>,
}

/// How one seeded resolve ended; anything but a landing left the lock as the seed found it.
enum SeedOutcome {
    Landed(SeededResolve),
    /// The resolve broke the [acceptance rule](acceptance) and was restored.
    Violated(Violation),
    /// Cargo refused the seed, or it was not attempted.
    Refused,
}

impl SeedOutcome {
    fn into_landed(self) -> Option<SeededResolve> {
        match self {
            SeedOutcome::Landed(seeded) => Some(seeded),
            SeedOutcome::Violated(_) | SeedOutcome::Refused => None,
        }
    }
}

impl Violation {
    /// The planned moves that would heal this violation, when it is a split of a declared crate.
    fn heal(&self) -> Option<SplitHeal> {
        match self {
            Violation::DirectSplit {
                name, retainers, ..
            } => Some(SplitHeal {
                name: name.clone(),
                retainers: retainers.clone(),
            }),
            Violation::DeclaredMoved { .. }
            | Violation::PlannedDrifted { .. }
            | Violation::TargetDisplaced { .. } => None,
        }
    }

    /// The names of the planned crates a violation points at, whose own moves broke the rule.
    ///
    /// A displaced target names no culprit: some other move of the step pushed it off.
    fn culprits(&self) -> BTreeSet<&str> {
        match self {
            Violation::DeclaredMoved { name, .. } | Violation::PlannedDrifted { name, .. } => {
                BTreeSet::from([name.as_str()])
            }
            Violation::DirectSplit { name, movers, .. } => movers
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(name.as_str()))
                .collect(),
            Violation::TargetDisplaced { .. } => BTreeSet::new(),
        }
    }

    /// The other planned crate whose own plan the violation enforced against `change`'s step.
    fn planned_constraint(&self, change: &Change) -> Option<&str> {
        match self {
            Violation::PlannedDrifted { name, .. } | Violation::TargetDisplaced { name, .. }
                if *name != change.package.name =>
            {
                Some(name)
            }
            _ => None,
        }
    }

    /// The `changes` the violation names and the rest, when both are non-empty.
    fn split_culprits<'a>(&self, changes: &[&'a Change]) -> Option<CulpritSplit<'a>> {
        let culprits = self.culprits();
        let (named, rest): (Vec<&Change>, Vec<&Change>) = changes
            .iter()
            .copied()
            .partition(|change| culprits.contains(change.package.name.as_str()));
        (!named.is_empty() && !rest.is_empty()).then_some(CulpritSplit { named, rest })
    }
}

/// The changes of a group a violation names, and the rest of the group.
struct CulpritSplit<'a> {
    named: Vec<&'a Change>,
    rest: Vec<&'a Change>,
}

#[derive(Default)]
struct WholeGraphStats {
    rounds: usize,
    joint_probes: usize,
    /// Joint probes repeated without the candidates Cargo named in the previous probe's refusal.
    joint_retries: usize,
    joint_full_successes: usize,
    partial_replays_kept: usize,
    partial_replays_discarded: usize,
    per_change_attempts: usize,
    memo: MemoStats,
}

struct ResolveCounter<'a> {
    observer: Option<&'a dyn ApplyObserver>,
    invocations: AtomicUsize,
}

impl ApplyObserver for ResolveCounter<'_> {
    fn resolver_started(&self, change: Option<&Change>) {
        self.invocations.fetch_add(1, Ordering::Relaxed);
        if let Some(observer) = self.observer {
            observer.resolver_started(change);
        }
    }

    fn candidate_started(&self, change: &Change) {
        if let Some(observer) = self.observer {
            observer.candidate_started(change);
        }
    }
}

#[derive(Clone, Copy)]
struct WidenProbe<'a> {
    project: &'a Project,
    /// Every change of the plan, which each widen's [acceptance rule](acceptance) is judged
    /// against.
    plan: &'a [Change],
    /// The plan's rewrite mode, which decides whether an already admitting requirement is kept.
    rewrite: RewriteMode,
    candidates: &'a [&'a Change],
    protected: &'a [&'a Change],
    followers: &'a [MemberRef],
    journal: &'a ProjectMutationJournal,
    observer: Option<&'a dyn ApplyObserver>,
}

impl<'a> WidenProbe<'a> {
    fn scope(&self) -> PinScope<'a> {
        PinScope {
            project: self.project,
            plan: self.plan,
            journal: self.journal,
            observer: self.observer,
        }
    }

    /// The first way `after` breaks the [acceptance rule](acceptance) against `baseline`.
    fn violation(&self, baseline: &ResolveBaseline, after: &CargoLock) -> Option<Violation> {
        baseline.violation(after, &self.project.generated_members, self.plan)
    }

    /// Every way `after` breaks the [acceptance rule](acceptance) against `baseline`, first to
    /// last.
    fn violations<'s>(
        &self,
        baseline: &'s ResolveBaseline,
        after: &'s CargoLock,
    ) -> impl Iterator<Item = Violation> + 's
    where
        'a: 's,
    {
        baseline.violations(after, &self.project.generated_members, self.plan)
    }

    fn metadata_mode(&self) -> MemoMetadataMode {
        if self
            .candidates
            .iter()
            .chain(self.protected)
            .any(|change| needs_member_graph(change))
        {
            MemoMetadataMode::Resolve
        } else {
            MemoMetadataMode::Skip
        }
    }
}

struct WidenEvidence {
    versions: BTreeMap<SlotKey, String>,
    graph: Option<ResolvedGraph>,
}

/// Which candidates a joint widen probe leaves out after a resolve breaks the
/// [acceptance rule](acceptance).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CulpritDrop {
    /// Those the first violation names, so the next resolve shows whether the rest still break
    /// the rule without them.
    First,
    /// Those every violation of the resolve names, at once (`upgrade --fast`).
    ///
    /// It saves a resolve per further culprit, but may leave out a candidate that would have
    /// landed once an earlier culprit was gone.
    /// A candidate left out still gets its own per-change attempt.
    All,
}

impl CulpritDrop {
    /// The culprits a joint probe for `plan` drops: all at once under `upgrade --fast`.
    fn of(plan: &Plan) -> Self {
        if plan.fast { Self::All } else { Self::First }
    }

    /// The outcome of a resolve that broke the acceptance rule in `violations`, with the culprits
    /// this mode drops, or `None` when it broke none.
    fn violated(self, mut violations: impl Iterator<Item = Violation>) -> Option<GroupResolve> {
        let violation = violations.next()?;
        tracing::debug!(%violation, "cargo joint widen broke the acceptance rule");
        let mut culprits: BTreeSet<String> = violation
            .culprits()
            .into_iter()
            .map(str::to_string)
            .collect();
        // Only a probe that drops them all reads the rest, so only it pays for finding them.
        if self == Self::All {
            for further in violations {
                culprits.extend(further.culprits().into_iter().map(str::to_string));
            }
        }
        Some(GroupResolve::Violated {
            violation,
            culprits,
        })
    }

    /// The `candidates` a probe retries without after `outcome` refused or violated the group.
    fn dropped<'c>(self, outcome: GroupResolve, candidates: &[&'c Change]) -> Vec<&'c Change> {
        match outcome {
            GroupResolve::Refused { parties, chain } => {
                refused_candidates(candidates, &parties, &chain)
            }
            // A violation that names the planned crates whose own moves broke the rule retries
            // without them like a refusal naming them.
            GroupResolve::Violated {
                violation,
                culprits,
            } => match self {
                Self::First => violation
                    .split_culprits(candidates)
                    .map(|split| split.named)
                    .unwrap_or_default(),
                Self::All => candidates
                    .iter()
                    .copied()
                    .filter(|change| culprits.contains(&change.package.name))
                    .collect(),
            },
            GroupResolve::Resolved(_) | GroupResolve::Discarded => Vec::new(),
        }
    }
}

/// The outcome of resolving one widened group.
enum GroupResolve {
    /// The group resolved within the acceptance rule.
    Resolved(Box<WidenEvidence>),
    /// Cargo refused the group, naming the packages it could not select and every package its
    /// explanation cites (both empty for a refusal it does not explain).
    Refused {
        parties: Vec<ConflictParty>,
        chain: BTreeSet<String>,
    },
    /// The resolve broke the acceptance rule.
    Violated {
        /// The first way it broke the rule.
        violation: Violation,
        /// The planned crates the violations the resolve's [`CulpritDrop`] selects name as
        /// culprits: the first violation's, or every violation's.
        culprits: BTreeSet<String>,
    },
    /// The resolve moved a seeded package to a version the plan does not name, or the seed could
    /// not be written.
    Discarded,
}

impl GroupResolve {
    fn into_evidence(self) -> Option<WidenEvidence> {
        match self {
            Self::Resolved(evidence) => Some(*evidence),
            Self::Refused { .. } | Self::Violated { .. } | Self::Discarded => None,
        }
    }
}

impl WidenEvidence {
    fn reached(&self, change: &Change) -> bool {
        reached_after(&self.versions, self.graph.as_ref(), change)
    }
}

struct WidenRound<'a> {
    evidence: WidenEvidence,
    protected: Vec<&'a Change>,
}

impl<'a> WidenRound<'a> {
    fn new(project: &Project, changes: &'a [Change], graph: Option<ResolvedGraph>) -> Result<Self> {
        let evidence = WidenEvidence {
            versions: read_lock(project)?.crates_io_locked_versions(),
            graph,
        };
        let protected = changes
            .iter()
            .filter(|change| evidence.reached(change))
            .collect();
        Ok(Self {
            evidence,
            protected,
        })
    }

    fn accept(&mut self, evidence: WidenEvidence, changes: &'a [Change]) {
        self.protected = changes
            .iter()
            .filter(|change| evidence.reached(change))
            .collect();
        self.evidence = evidence;
    }
}

fn widen_checkpoint(
    project: &Project,
    candidates: &[&Change],
    followers: &[MemberRef],
) -> Result<ProjectMutationJournal> {
    let mut paths: BTreeSet<_> = candidates
        .iter()
        .flat_map(|change| &change.members)
        .chain(followers)
        .map(|member| manifest::member_manifest_rel(&member.path))
        .collect();
    paths.insert(Utf8PathBuf::from("Cargo.toml"));
    paths.insert(Utf8PathBuf::from("Cargo.lock"));
    ProjectMutationJournal::capture(&project.root, paths)
}

fn restore_widen_checkpoint(
    project: &Project,
    journal: &ProjectMutationJournal,
    checkpoint: &ProjectMutationJournal,
) -> Result<()> {
    journal.validate_project(&project.root)?;
    checkpoint.validate_project(&project.root)?;
    checkpoint.restore()
}

struct WidenFingerprint {
    manifests: String,
    lock: String,
}

enum TentativeWiden {
    Unchanged,
    Landed(Box<WidenEvidence>),
    Rejected,
}

#[derive(Debug)]
enum PrecisePinOutcome {
    Completed,
    Rejected {
        detail: Option<String>,
        resolver_rejection: bool,
        /// The packages Cargo's conflict set against the pin, when it reported one.
        parties: Vec<ConflictParty>,
        /// The split of a declared crate the pin's landing caused, when that refused it.
        split: Option<SplitHeal>,
    },
}

/// The apply-wide context every pin batch runs in.
#[derive(Clone, Copy)]
struct PinScope<'a> {
    project: &'a Project,
    /// Every change of the plan being applied, which each resolve step's
    /// [acceptance rule](acceptance) is judged against.
    plan: &'a [Change],
    journal: &'a ProjectMutationJournal,
    observer: Option<&'a dyn ApplyObserver>,
}

/// The context every precise pin of one pin batch runs in.
#[derive(Clone, Copy)]
struct PinPass<'a> {
    scope: PinScope<'a>,
    /// Every obligation of the running pin batch, whose crates reach their own targets through
    /// their own pins and so are never released as another pin's blockers.
    batch: &'a [&'a Change],
}

/// A precise pin Cargo refused, with what its refusal said.
struct RefusedPin<'a> {
    change: &'a Change,
    /// The locked version the pin addresses.
    current: &'a str,
    detail: Option<String>,
    resolver_rejection: bool,
    parties: Vec<ConflictParty>,
    split: Option<SplitHeal>,
}

/// A refused precise pin's identity: the project root and the change's `(name, from, to)`.
type RefusalKey = (Utf8PathBuf, (String, String, String));

/// What releasing a refused pin's blockers achieved.
enum Unlocking {
    Landed,
    /// Every attempt was refused, the last one for the split it would cause when that is why.
    Refused(Option<UnlockedSplit>),
    /// The refusal named no blocker the batch may release, so no unlocking was attempted.
    NothingToRelease,
}

/// A split of a declared crate an unlocking pin caused while releasing `released`.
struct UnlockedSplit {
    heal: SplitHeal,
    released: Vec<PackageKey>,
}

/// What [`CargoTool::complete_split`] achieved.
enum SplitCompletion {
    /// The step landed together with the planned moves that heal its split.
    Landed(Box<WidenEvidence>),
    /// No planned move heals the split, so no resolve was tried.
    Unhealable,
    /// The healing moves were tried together and did not land.
    Refused,
}

/// A split completion's identity within one resolve: the project root, a digest of the project
/// files it starts from, the coalition and the changes it must not displace, the rewrite mode its
/// widens use, and the locked nodes it releases.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CompletionKey {
    root: Utf8PathBuf,
    state: [u8; 32],
    coalition: BTreeSet<(String, String, String)>,
    protected: BTreeSet<(String, String, String)>,
    rewrite: RewriteMode,
    /// The locked nodes the completion releases, which change the commands it runs.
    released: BTreeSet<(String, String)>,
}

/// How many rounds of newly named blockers one refused pin may release before it stays held.
const PIN_BLOCKER_ROUNDS: usize = 3;

/// How many rounds of newly named blockers a conflicted lock refresh may release before it fails.
/// Each round releases at least one more node Cargo named, so a family that moves together (the
/// four pest crates and the `psm` they pin) settles well inside it.
const LOCK_CONFLICT_ROUNDS: usize = 8;

fn widen_fingerprint(
    project: &Project,
    journal: &ProjectMutationJournal,
) -> Result<Option<WidenFingerprint>> {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return Ok(None);
    }
    let JournaledDigests { manifests, lock } = journaled_digests(project, journal)?;
    Ok(Some(WidenFingerprint {
        manifests: format!("{:02x?}", manifests.finalize().as_slice()),
        lock: format!("{:02x?}", lock.finalize().as_slice()),
    }))
}

/// A digest of every journaled project file as it is on disk now.
fn journaled_state(project: &Project, journal: &ProjectMutationJournal) -> Result<[u8; 32]> {
    let JournaledDigests { manifests, lock } = journaled_digests(project, journal)?;
    let mut state = Sha256::new();
    state.update(manifests.finalize());
    state.update(lock.finalize());
    Ok(state.finalize().into())
}

struct JournaledDigests {
    manifests: Sha256,
    lock: Sha256,
}

/// The digests of the journaled manifests and of the lock, each over every file's path, presence
/// and contents in path order.
fn journaled_digests(
    project: &Project,
    journal: &ProjectMutationJournal,
) -> Result<JournaledDigests> {
    let mut manifests = Sha256::new();
    let mut lock = Sha256::new();
    let mut files: Vec<_> = journal.files().iter().collect();
    files.sort_by_key(|file| file.path());
    for file in files {
        let path = file.path();
        let contents = match std::fs::read(project.root.join(path)) {
            Ok(contents) => Some(contents),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        let hash = if path == Utf8Path::new("Cargo.lock") {
            &mut lock
        } else {
            &mut manifests
        };
        hash.update(path.as_str().len().to_le_bytes());
        hash.update(path.as_str().as_bytes());
        hash.update([u8::from(contents.is_some())]);
        if let Some(contents) = contents {
            hash.update(contents.len().to_le_bytes());
            hash.update(contents);
        }
    }
    Ok(JournaledDigests { manifests, lock })
}

/// Rejects seeded slots that resolve to neither their original nor requested version.
fn seed_has_drive_by(before: &CargoLock, after: &CargoLock, moves: &[PlannedNodeMove]) -> bool {
    moves.iter().any(|planned| {
        let unexpected_slot = after.package.iter().any(|package| {
            package.name == planned.name
                && package.source.as_deref() == Some(crate::lockfile::CRATES_IO_SOURCE)
                && package.version.as_ref().is_some_and(|resolved| {
                    resolved != &planned.from
                        && resolved != &planned.to
                        && !before.has_crates_io_package(&planned.name, resolved)
                        && (compatibility_line(resolved) == compatibility_line(&planned.from)
                            || compatibility_line(resolved) == compatibility_line(&planned.to))
                })
        });
        unexpected_slot
            || (!after.has_crates_io_package(&planned.name, &planned.from)
                && !after.has_crates_io_package(&planned.name, &planned.to))
    })
}

/// The planned, unreached changes of `plan` that would heal `split`: the move of the split crate
/// itself, and the moves of the requirers still binding its old line, other than `coalition`'s.
fn split_partners<'a>(
    plan: &'a [Change],
    lock: &CargoLock,
    split: &SplitHeal,
    coalition: &[&Change],
) -> Vec<&'a Change> {
    plan.iter()
        .filter(|planned| {
            !coalition.contains(planned)
                && current_selector(lock, planned).is_some()
                && (planned.package.name == split.name
                    || split.retainers.iter().any(|retainer| {
                        retainer.name == planned.package.name
                            && retainer.version == planned.from.as_str()
                    }))
        })
        .collect()
}

/// The candidates of a refused joint group the refusal names: those Cargo could not select, or
/// failing any, those its explanation cites.
fn refused_candidates<'a>(
    candidates: &[&'a Change],
    parties: &[ConflictParty],
    chain: &BTreeSet<String>,
) -> Vec<&'a Change> {
    let party = |change: &&Change| {
        parties
            .iter()
            .any(|party| party.name == change.package.name)
    };
    if candidates.iter().any(party) {
        candidates.iter().copied().filter(party).collect()
    } else {
        candidates
            .iter()
            .copied()
            .filter(|change| chain.contains(&change.package.name))
            .collect()
    }
}

/// The lock moves a seed of `changes` writes: every change whose target `lock` does not carry yet.
fn seed_moves(lock: &CargoLock, changes: &[&Change]) -> Vec<PlannedNodeMove> {
    changes
        .iter()
        .filter(|change| !lock.has_crates_io_package(&change.package.name, change.to.as_str()))
        .map(|change| PlannedNodeMove {
            name: change.package.name.clone(),
            from: change.from.to_string(),
            to: change.to.to_string(),
        })
        .collect()
}

/// Logs one per-change pin attempt with the inputs it was judged against.
fn log_pin_attempt(
    change: &Change,
    fingerprint: Option<&WidenFingerprint>,
    memo_hit: bool,
    outcome: &str,
) {
    tracing::debug!(
        change = ?rejection_key(change),
        members = ?change.members,
        manifest_hash = fingerprint.map(|hash| hash.manifests.as_str()),
        lock_hash = fingerprint.map(|hash| hash.lock.as_str()),
        memo_hit,
        outcome,
        "cargo per-change pin attempt"
    );
}

/// The [`PinRejections`] key of one planned change: its `(name, from, to)` line.
fn rejection_key(change: &Change) -> (String, String, String) {
    (
        change.package.name.clone(),
        change.from.as_str().to_string(),
        change.to.as_str().to_string(),
    )
}

/// Each planned candidate either reached cooldown's target (its newest-within-window) — reported
/// applied — or fell short because a mutually-exclusive `=`-pin or single-major shared transitive
/// won — reported held, naming the blocker.
fn classify_planned_changes(
    plan: &Plan,
    crates_io_after: &BTreeMap<SlotKey, String>,
    graph: Option<&crate::cargocmd::ResolvedGraph>,
    pin_rejections: &PinRejections,
    report: &mut ApplyReport,
) {
    for change in &plan.changes {
        if reached_after(crates_io_after, graph, change) {
            report.applied.push(change.clone());
        } else {
            let offender = graph
                .and_then(|graph| {
                    blocking_requirer(graph, &change.package.name, change.to.as_str())
                })
                .unwrap_or_else(|| change.package.name.clone());
            // Cargo's own rejection sentence (which requirement, declared by whom) beats
            // both the generic reason and the `=`-pin-only graph offender.
            let detail = pin_rejections.get(&rejection_key(change)).cloned();
            report.skipped.push(Skipped {
                change: change.clone(),
                // A pin cargo itself refused is the resolver's rejection, and its sentence says
                // why; a candidate the resolve merely left short is capped by another crate's
                // requirement, a hold with a different way forward.
                reason: if detail.is_some() {
                    SkipReason::ResolverConflict
                } else {
                    SkipReason::GraphCeilingHeld
                },
                offending: Some(PackageId::new(
                    CARGO_ID,
                    offender,
                    Some(CRATES_IO.to_string()),
                )),
                detail,
            });
        }
    }
}

/// The files a candidate's tentative widen may touch — its members' manifests plus the workspace
/// root's (the inherited-requirement fallback), and the staged `Cargo.lock` — with their pre-widen
/// contents, so a widen whose pin cannot land is restored byte-identically instead of leaving a
/// requirement the lock does not honor.
///
/// The lock belongs in this snapshot because the rollback runs *after* the unlocked
/// `cargo metadata` probe, which can itself re-lock the widened requirement — forking a major that
/// no requirement admits once the manifests are restored. Restoring the manifests alone would
/// leave that poisoned lock for `resolver_lock` and the preserve preflight to observe until a
/// later unlocked resolve incidentally healed it.
fn widen_snapshot(
    root: &Utf8Path,
    members: &[MemberRef],
    followers: &[MemberRef],
) -> Result<Vec<(Utf8PathBuf, Option<String>)>> {
    let mut paths: Vec<Utf8PathBuf> = members
        .iter()
        .chain(followers)
        .map(|member| manifest::member_manifest_rel(&member.path))
        .collect();
    paths.push(Utf8PathBuf::from("Cargo.toml"));
    paths.push(Utf8PathBuf::from("Cargo.lock"));
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .map(|rel| {
            let contents = match std::fs::read_to_string(root.join(&rel)) {
                Ok(contents) => Some(contents),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(CoreError::from(error)),
            };
            Ok((rel, contents))
        })
        .collect()
}

/// Writes a [`widen_snapshot`] back verbatim. A path captured as absent is left alone — the
/// widen machinery never creates manifests or locks.
fn restore_widen_snapshot(
    root: &Utf8Path,
    snapshot: &[(Utf8PathBuf, Option<String>)],
    journal: &ProjectMutationJournal,
) -> Result<()> {
    for (rel, contents) in snapshot {
        if let Some(contents) = contents {
            journal.validate_project(root)?;
            std::fs::write(root.join(rel), contents)?;
        }
    }
    Ok(())
}

/// Compresses a rejected `cargo update --precise` stderr into one report-friendly line: cargo's
/// `error:` sentence plus the first `required by package` attribution — the two facts that say
/// which requirement blocked the pin and whose manifest declares it. `None` when the error carries
/// no stderr (nothing better than the generic reason exists). The summary reaches the TTY and
/// JSON reports verbatim, so embedded URLs are stripped of credentials here, at construction.
fn summarize_pin_rejection(error: &CoreError) -> Option<String> {
    use std::fmt::Write as _;
    let CoreError::Tool { stderr, .. } = error else {
        return None;
    };
    let sentence = stderr.lines().map(str::trim).find_map(|line| {
        line.strip_prefix("error: ")
            .or_else(|| line.strip_prefix("error["))
            .map(|rest| rest.trim_end_matches('.'))
    })?;
    let mut summary = sentence.to_string();
    // Cargo prints the first attribution either bare (`required by package …`) or as a chain
    // continuation (`... required by package …`); accept both.
    if let Some(requirer) = stderr.lines().map(str::trim).find_map(|line| {
        line.trim_start_matches('.')
            .trim_start()
            .strip_prefix("required by package `")
            .and_then(|rest| rest.split('`').next())
    }) {
        // The path/hash suffix cargo prints for workspace members is noise at report width.
        let requirer = requirer.split(" (").next().unwrap_or(requirer);
        let _ = write!(summary, " (required by {requirer})");
    }
    // Cargo quotes registry URLs inside requirement errors; a private-registry URL can embed
    // credentials, so redact before the cap bounds the (possibly lengthened) line.
    let mut summary = cooldown_core::redact::url_secrets(&summary);
    // Char-boundary-safe cap: the requirement sentence quotes tool-controlled strings.
    if let Some((cut, _)) = summary.char_indices().nth(220) {
        summary.truncate(cut);
        summary.push('…');
    }
    Some(summary)
}

/// The crate whose `=x.y.z` requirement structurally holds `held` out of the graph at `target` —
/// the cargo analog of uv's `blocking_requirer`. A held cargo candidate is almost always blocked by
/// an exact pin on a shared single-major node (cargo coexists distinct majors, so an open caret
/// range rarely conflicts): some *other* crate's `graph_ceiling` (an active `=` edge) caps the
/// shared node below the candidate's target. Returns the requirer that caps `held`, or `None` so the
/// caller falls back to the unattributed graph-ceiling message.
fn blocking_requirer(
    graph: &crate::cargocmd::ResolvedGraph,
    held: &str,
    target: &str,
) -> Option<String> {
    // A workspace member's own exact pin holds the candidate: name the member.
    let pinned_below = graph
        .exact_pins
        .iter()
        .any(|(name, pinned)| name == held && version::compare(target, pinned).is_gt());
    if pinned_below {
        // The held crate is exact-pinned below its target by the project itself; the project is the
        // blocker, but naming the crate itself yields the generic message, which is correct here.
        return None;
    }
    // Some requirer caps the shared `held` node with an active `=` edge below the target: find the
    // crate that declares that exact requirement (its edge resolves to a `held` node).
    let blocker = graph.exact_requirer_of(held, target);
    blocker.filter(|name| name != held)
}

/// Every package a resolver refusal's explanation cites, or none for any other failure.
fn rejection_chain(error: &CoreError) -> BTreeSet<String> {
    match error {
        CoreError::Tool { stderr, .. } if memoizable_resolver_rejection(error) => {
            crate::cargocmd::diagnostic_packages(stderr)
        }
        _ => BTreeSet::new(),
    }
}

/// The parties of a resolver conflict `error` reports, or none for any other refusal.
fn rejection_parties(error: &CoreError) -> Vec<ConflictParty> {
    match error {
        CoreError::Tool { stderr, .. } if memoizable_resolver_rejection(error) => {
            conflict_parties(stderr)
        }
        _ => Vec::new(),
    }
}

/// The crates.io nodes of `lock` that `parties` name: each party at its stated version, or at the
/// only version the lock carries of it when the diagnostic stated none.
fn locked_parties(lock: &CargoLock, parties: &[ConflictParty]) -> Vec<PackageKey> {
    let mut nodes: Vec<PackageKey> = Vec::new();
    for party in parties {
        let version = match &party.version {
            Some(version) if lock.has_crates_io_package(&party.name, version) => version.clone(),
            Some(_) => continue,
            None => match lock.crates_io_versions(&party.name).as_slice() {
                [only] => (*only).to_string(),
                _ => continue,
            },
        };
        let node = PackageKey::new(party.name.clone(), version);
        if !nodes.contains(&node) {
            nodes.push(node);
        }
    }
    nodes
}

/// The locked nodes of a refused pin's conflict other than the candidate itself.
fn conflicting_nodes(
    lock: &CargoLock,
    change: &Change,
    parties: &[ConflictParty],
) -> Vec<PackageKey> {
    locked_parties(lock, parties)
        .into_iter()
        .filter(|node| node.name != change.package.name)
        .collect()
}

/// The locked nodes a refused pin of `change` may release: the [`conflicting_nodes`] that are
/// neither planned nor declared by an authored package.
///
/// A planned crate reaches its own target through its own pin.
/// A directly declared node is never moved outside the plan (see [`acceptance`]), so releasing
/// one would only buy a landing the rule then refuses.
/// The planned crates other than `change` among the conflict parties [`pin_blockers`] withholds
/// because `batch` plans them.
fn withheld_planned(
    lock: &CargoLock,
    change: &Change,
    batch: &[&Change],
    parties: &[ConflictParty],
) -> BTreeSet<String> {
    conflicting_nodes(lock, change, parties)
        .into_iter()
        .filter(|node| {
            node.name != change.package.name
                && batch
                    .iter()
                    .any(|planned| planned.package.name == node.name)
        })
        .map(|node| node.name)
        .collect()
}

fn pin_blockers(
    lock: &CargoLock,
    baseline: &ResolveBaseline,
    change: &Change,
    batch: &[&Change],
    parties: &[ConflictParty],
) -> Vec<PackageKey> {
    conflicting_nodes(lock, change, parties)
        .into_iter()
        .filter(|node| {
            !baseline.declares_crates_io(&node.name, &node.version)
                && batch
                    .iter()
                    .all(|planned| planned.package.name != node.name)
        })
        .collect()
}

impl CargoTool {
    fn rejection_memo_key(&self, attempt: &MemoAttempt<'_>) -> Result<Option<MemoKey>> {
        attempt.validate()?;
        if self.rejection_memo_enabled {
            match attempt.key(&self.rejection_memo) {
                Ok(key) if key.is_cacheable() => Ok(Some(key)),
                Ok(_) => Ok(None),
                Err(err) => {
                    tracing::debug!(operation = ?attempt.operation, %err, "cargo rejection memo input capture bypassed");
                    Ok(None)
                }
            }
        } else {
            Ok(None)
        }
    }

    fn recall_rejection(
        &self,
        attempt: &MemoAttempt<'_>,
        key: Option<&MemoKey>,
        stats: &mut MemoStats,
    ) -> Result<Option<MemoRejection>> {
        // A hit has no subprocess to perform the supervisor's interruption check.
        attempt.validate()?;
        let Some(key) = key else {
            return Ok(None);
        };
        let mut rejection = self.rejection_memo.get(key);
        if let Some(recalled) = &rejection
            && !recalled.heals.is_empty()
        {
            let lock = read_lock(attempt.project)?;
            if recalled
                .heals
                .iter()
                .any(|heal| !split_partners(attempt.plan, &lock, heal, attempt.changes).is_empty())
            {
                rejection = None;
            }
        }
        if rejection.is_some() {
            stats.hits += 1;
        } else {
            stats.misses += 1;
        }
        Ok(rejection)
    }

    fn remember_rejection(
        &self,
        attempt: &MemoAttempt<'_>,
        before: Option<MemoKey>,
        rejection: MemoRejection,
    ) -> Result<()> {
        if let Some(before) = before
            && self.inputs_restored(attempt, &before)?
        {
            self.rejection_memo.insert(before, rejection);
        }
        Ok(())
    }

    /// Whether `attempt` left every native input as it found it, which is what makes its refusal
    /// reusable: an input it changed and did not restore belongs to a state no later attempt starts
    /// from.
    fn inputs_restored(&self, attempt: &MemoAttempt<'_>, before: &MemoKey) -> Result<bool> {
        attempt.validate()?;
        Ok(self.rejection_memo_key(attempt)?.as_ref() == Some(before))
    }

    /// Revalidates `mutation` through whichever execution mode this platform selected.
    ///
    /// The two accessors carry the same parts but enforce different capabilities, so the choice
    /// has to track [`ToolWrite::mutation_execution`] exactly or every dispatch fails closed.
    fn mutation_parts<'a>(
        &self,
        mutation: &'a PreparedMutation,
    ) -> Result<(&'a Project, &'a Plan, &'a ProjectMutationJournal)> {
        match self.mutation_execution() {
            MutationExecution::Isolated(_) => mutation.isolated_parts_for(self),
            MutationExecution::InPlace => mutation.parts_for(self),
        }
    }

    /// Re-resolves the **whole** graph under cooldown's window.
    ///
    /// `upgrade` is informational for the rewrite policy.
    /// Cargo has no date cutoff, so each planned target is expressed as a concrete `--precise` pin
    /// computed by the core.
    /// Under `Always`, every owning constraint is widened up front, before the pin batch.
    /// Under `Auto`, the pin batch runs first, then every widen round tries its remaining targets
    /// jointly and replays partial landings with only their own edits.
    /// Targets still short get a tentative individual widen, kept when the re-pin lands and
    /// restored when it does not.
    #[cfg(all(test, unix))]
    async fn whole_graph_resolve(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<PinRejections> {
        self.resolve_whole_graph(project, plan, journal, observer)
            .await
            .map(|resolved| resolved.rejections)
    }

    /// [`Self::whole_graph_resolve`], with what the resolve settled for attesting its holds.
    async fn resolve_whole_graph(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<WholeGraphResolve> {
        let counter = ResolveCounter {
            observer,
            invocations: AtomicUsize::new(0),
        };
        let mut stats = WholeGraphStats::default();
        let result = self
            .whole_graph_resolve_inner(project, plan, journal, Some(&counter), &mut stats)
            .await;
        tracing::debug!(
            rounds = stats.rounds,
            joint_probes = stats.joint_probes,
            joint_retries = stats.joint_retries,
            joint_full_successes = stats.joint_full_successes,
            partial_replays_kept = stats.partial_replays_kept,
            partial_replays_discarded = stats.partial_replays_discarded,
            per_change_attempts = stats.per_change_attempts,
            memo_hits = stats.memo.hits,
            memo_misses = stats.memo.misses,
            cargo_invocations = counter.invocations.load(Ordering::Relaxed),
            outcome = if result.is_ok() { "resolved" } else { "error" },
            "cargo whole graph resolve finished"
        );
        result
    }

    async fn whole_graph_resolve_inner(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
        stats: &mut WholeGraphStats,
    ) -> Result<WholeGraphResolve> {
        let followers = self.pin_followers(project).await?;
        self.forget_resolve_state(&project.root);
        let start = self.landing_start(project, journal, &followers)?;
        let warm = match &start {
            Some(start) => self.warm_start(project, plan, journal, start)?,
            None => None,
        };
        // Widen the owning manifest constraints for all candidates up front under `Always`; under
        // `Auto`, widen only those whose own declared requirement would otherwise cap them below the
        // target (a cross-major bump). The pin itself follows.
        if matches!(plan.rewrite, RewriteMode::Always) {
            widen_all_up_front(project, plan, journal, &followers)?;
        }
        let mut rejections = BTreeMap::new();
        let scope = PinScope {
            project,
            plan: &plan.changes,
            journal,
            observer,
        };
        self.pin_batch_with_memo_stats(scope, &plan.changes, &mut rejections, &mut stats.memo)
            .await?;

        if matches!(plan.rewrite, RewriteMode::Auto) {
            self.widen_in_rounds(project, plan, journal, observer, &mut rejections, stats)
                .await?;
        }
        self.land_rejected_group_atomically(
            project,
            plan,
            journal,
            observer,
            &mut rejections,
            &mut stats.memo,
        )
        .await?;
        self.reconcile_projections(scope).await?;
        let mut unmoved = None;
        if let Some(start) = start {
            let files = self.remember_landing(project, plan, &start)?;
            // A resolve that ended on its warm start's files landed nothing beyond it, so every
            // refusal it met was met in that one state.
            if files.is_some() && files == warm {
                unmoved = Some(UnmovedResolve {
                    key: start.key,
                    lock: start.lock,
                    files: files.unwrap_or_default(),
                });
            }
        }
        Ok(WholeGraphResolve {
            rejections,
            unmoved,
        })
    }

    /// The key and files a whole-graph resolve starts from, when the run may reuse its landing.
    fn landing_start(
        &self,
        project: &Project,
        journal: &ProjectMutationJournal,
        followers: &[MemberRef],
    ) -> Result<Option<LandingStart>> {
        // The landing key never recalls a rejection, so no plan guards it.
        let attempt = MemoAttempt {
            project,
            plan: &[],
            operation: MemoOperation::Landing,
            metadata_mode: MemoMetadataMode::Skip,
            changes: &[],
            followers,
            journal,
        };
        let Some(key) = self.rejection_memo_key(&attempt)? else {
            return Ok(None);
        };
        let mut files = Vec::new();
        for file in journal.files() {
            let contents = match std::fs::read(project.root.join(file.path())) {
                Ok(contents) => Some(contents),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(CoreError::from(error)),
            };
            files.push((file.path().to_path_buf(), contents));
        }
        Ok(Some(LandingStart {
            key,
            lock: read_lock(project)?,
            files,
        }))
    }

    /// Writes the landing of an earlier resolve that `plan` may start from, if one is recorded,
    /// and returns its files.
    fn warm_start(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        start: &LandingStart,
    ) -> Result<Option<Vec<(Utf8PathBuf, String)>>> {
        let Some(warm) = self.landings.recall(
            &start.key,
            &start.lock,
            plan.rewrite,
            &plan.changes,
            journal,
            &project.generated_members,
        ) else {
            return Ok(None);
        };
        for (path, contents) in &warm.files {
            journal.validate_project(&project.root)?;
            std::fs::write(project.root.join(path), contents)?;
        }
        tracing::debug!(
            touched = warm.touched,
            files = warm.files.len(),
            "cargo whole graph resolve warm-started from an earlier landing"
        );
        Ok(Some(warm.files))
    }

    /// Records where the resolve that began at `start` ended, for later applies of the run, and
    /// returns the files it changed, unless one is no longer text.
    fn remember_landing(
        &self,
        project: &Project,
        plan: &Plan,
        start: &LandingStart,
    ) -> Result<Option<Vec<(Utf8PathBuf, String)>>> {
        let mut files = Vec::new();
        for (path, before) in &start.files {
            let after = match std::fs::read(project.root.join(path)) {
                Ok(contents) => Some(contents),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(CoreError::from(error)),
            };
            if after == *before {
                continue;
            }
            // Only rewritten text files are replayed; a landing that removed or garbled one is
            // not reused.
            let Some(text) = after.and_then(|bytes| String::from_utf8(bytes).ok()) else {
                return Ok(None);
            };
            files.push((path.clone(), text));
        }
        self.landings.remember(landing::Finished {
            start: start.key.clone(),
            rewrite: plan.rewrite,
            plan: &plan.changes,
            start_lock: &start.lock,
            lock: read_lock(project)?,
            files: files.clone(),
        });
        Ok(Some(files))
    }

    async fn widen_in_rounds(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
        rejections: &mut PinRejections,
        stats: &mut WholeGraphStats,
    ) -> Result<()> {
        // Widen only the candidates the pin batch could not place at their target because their
        // own declared requirement caps them, then re-pin. Each widen is *tentative*: a widened
        // requirement whose pin still cannot land (a third-party crate holds the old major)
        // would leave the manifest demanding a version the lock does not carry, and that poisons
        // the whole batch at lock verification (`--locked`) — so a widen whose candidate stays
        // short is restored, and the candidate remains a held skip carrying its recorded
        // rejection. A short candidate whose widen is a *no-op* (its own requirement already
        // admits the target, or nothing declares it) may be held only by a sibling's
        // not-yet-widened requirement, so a round in which any widen+pin progresses re-pins
        // those candidates before it ends; a round that progresses nowhere proves the
        // remaining short candidates conflict with another crate (a real conflict the diff
        // reports), and only then does the loop stop widening.
        // The member-aware reach check is the only thing in this loop that needs the resolved
        // graph, so the initial preflight skips metadata when no candidate is a direct member
        // dep; every joint probe still resolves its seeded graph.
        // When a member graph is needed, fail closed: a lock-slot fallback is a false positive.
        let followers = self.pin_followers(project).await?;
        let graph = self
            .widen_round_graph(
                project,
                plan.changes.iter().any(needs_member_graph),
                journal,
                observer,
            )
            .await?;
        let scope = WidenProbe {
            project,
            plan: &plan.changes,
            rewrite: plan.rewrite,
            candidates: &[],
            protected: &[],
            followers: &followers,
            journal,
            observer,
        };
        let culprit_drop = CulpritDrop::of(plan);
        // Carry post-command evidence between rounds; another unlocked preflight could displace it.
        let mut round = WidenRound::new(project, &plan.changes, graph)?;
        for _ in 0..plan.changes.len() {
            let candidates: Vec<_> = plan
                .changes
                .iter()
                .filter(|change| !round.evidence.reached(change))
                .collect();
            if candidates.is_empty() {
                break;
            }
            stats.rounds += 1;
            // A lone candidate gains nothing from a joint probe: its own tentative widen below
            // widens, pins and resolves it alone.
            let retained = if candidates.len() > 1 {
                self.joint_widen_probe(
                    WidenProbe {
                        candidates: &candidates,
                        protected: &round.protected,
                        ..scope
                    },
                    culprit_drop,
                    stats,
                )
                .await?
            } else {
                None
            };
            let mut progressed = retained.is_some();
            if let Some(retained) = retained {
                round.accept(retained, &plan.changes);
            }
            let mut unwidened_short = Vec::new();
            for change in &plan.changes {
                if round.evidence.reached(change) {
                    continue;
                }
                stats.per_change_attempts += 1;
                let attempt = self
                    .tentative_widen_logged(
                        WidenProbe {
                            candidates: &candidates,
                            protected: &round.protected,
                            ..scope
                        },
                        change,
                        rejections,
                        &mut stats.memo,
                    )
                    .await?;
                match attempt {
                    TentativeWiden::Unchanged => unwidened_short.push(change.clone()),
                    TentativeWiden::Landed(evidence) => {
                        progressed = true;
                        round.accept(*evidence, &plan.changes);
                    }
                    TentativeWiden::Rejected => {}
                }
            }
            // A sibling's landed widen can remove the blocker behind an unwidened candidate.
            if progressed
                && !unwidened_short.is_empty()
                && let Some(evidence) = self
                    .pin_unwidened_protected(
                        WidenProbe {
                            candidates: &candidates,
                            protected: &round.protected,
                            ..scope
                        },
                        &unwidened_short,
                        rejections,
                        &mut stats.memo,
                    )
                    .await?
            {
                round.accept(evidence, &plan.changes);
            }
            if !progressed {
                break;
            }
        }
        Ok(())
    }

    async fn widen_round_graph(
        &self,
        project: &Project,
        needed: bool,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<Option<ResolvedGraph>> {
        if !needed {
            return Ok(None);
        }
        journal.validate_project(&project.root)?;
        let resolved = self.metadata_with_observer(project, observer).await;
        journal.validate_project(&project.root)?;
        resolved.map(Some)
    }

    // A probe never routes through pin_batch: lone-refusal memory belongs to its old context.
    async fn joint_widen_probe(
        &self,
        probe: WidenProbe<'_>,
        culprit_drop: CulpritDrop,
        stats: &mut WholeGraphStats,
    ) -> Result<Option<WidenEvidence>> {
        if probe.candidates.is_empty() {
            return Ok(None);
        }
        let mut candidates = probe.candidates.to_vec();
        candidates.sort_by_key(|change| rejection_key(change));
        probe.journal.validate_project(&probe.project.root)?;
        let owners: Vec<_> = candidates.iter().chain(probe.protected).copied().collect();
        let checkpoint = widen_checkpoint(probe.project, &owners, probe.followers)?;
        stats.joint_probes += 1;
        // One candidate Cargo cannot select (a target that dropped a feature a member enables)
        // refuses the whole group, so retry without the candidates the refusal names before
        // leaving every candidate to its own per-change attempt.
        // Cargo names the package it could not select, which may be a dependency of the widened
        // candidate (a `links` clash of its new `-sys` crate); the candidate then appears only
        // further up the explanation's chain.
        // A candidate dropped wrongly only falls back to its own attempt.
        // Each retry drops at least one candidate, which bounds the loop by the group size.
        let resolved = loop {
            match self
                .resolve_widened_group(&probe, &candidates, &[], culprit_drop)
                .await?
            {
                GroupResolve::Resolved(evidence) => break Some(*evidence),
                GroupResolve::Discarded => break None,
                outcome @ (GroupResolve::Refused { .. } | GroupResolve::Violated { .. }) => {
                    let dropped = culprit_drop.dropped(outcome, &candidates);
                    if dropped.is_empty() || dropped.len() == candidates.len() {
                        break None;
                    }
                    restore_widen_checkpoint(probe.project, probe.journal, &checkpoint)?;
                    candidates.retain(|change| !dropped.contains(change));
                    stats.joint_retries += 1;
                    tracing::debug!(
                        dropped = ?dropped.iter().map(|change| change.package.name.as_str()).collect::<Vec<_>>(),
                        remaining = candidates.len(),
                        "retrying cargo joint widen without the refused candidates"
                    );
                }
            }
        };
        let reached: Vec<_> = resolved
            .as_ref()
            .map(|resolved| {
                candidates
                    .iter()
                    .copied()
                    .filter(|change| resolved.reached(change))
                    .collect()
            })
            .unwrap_or_default();
        let displaced: Vec<_> = resolved
            .as_ref()
            .map(|resolved| {
                probe
                    .protected
                    .iter()
                    .filter(|change| !resolved.reached(change))
                    .map(|change| change.package.name.as_str())
                    .collect()
            })
            .unwrap_or_default();
        tracing::debug!(
            resolved = resolved.is_some(),
            candidates = candidates.len(),
            reached = reached.len(),
            ?displaced,
            "cargo joint widen probe finished"
        );
        if let Some(resolved) = resolved
            && reached.len() == candidates.len()
            && displaced.is_empty()
        {
            stats.joint_full_successes += 1;
            return Ok(Some(resolved));
        }
        restore_widen_checkpoint(probe.project, probe.journal, &checkpoint)?;
        if reached.is_empty() || reached.len() == candidates.len() {
            return Ok(None);
        }
        // Evidence from the full probe cannot authorize A without the failed B's edits.
        // Rebuild the subset from the checkpoint and resolve exactly those edits again.
        let replayed = self
            .resolve_widened_group(&probe, &reached, &[], culprit_drop)
            .await?
            .into_evidence();
        if let Some(replayed) = replayed
            && reached.iter().all(|change| replayed.reached(change))
            && probe
                .protected
                .iter()
                .all(|change| replayed.reached(change))
        {
            stats.partial_replays_kept += 1;
            return Ok(Some(replayed));
        }
        stats.partial_replays_discarded += 1;
        restore_widen_checkpoint(probe.project, probe.journal, &checkpoint)?;
        Ok(None)
    }

    /// Widens and seeds `candidates` together and resolves them in one step, also releasing the
    /// locked nodes `released` names so they may move wherever the candidates need them.
    ///
    /// A step that breaks the acceptance rule reports the culprits of the violations
    /// `culprit_drop` selects.
    async fn resolve_widened_group(
        &self,
        probe: &WidenProbe<'_>,
        candidates: &[&Change],
        released: &[PackageKey],
        culprit_drop: CulpritDrop,
    ) -> Result<GroupResolve> {
        let baseline =
            ResolveBaseline::capture(&read_lock(probe.project)?, &probe.project.generated_members);
        let mut edited = 0;
        for change in candidates {
            let rewrite = widen_and_follow_checked(
                &probe.project.root,
                change,
                probe.followers,
                probe.rewrite,
                || probe.journal.validate_project(&probe.project.root),
            )?;
            edited += usize::from(!rewrite.modified.is_empty());
        }
        tracing::debug!(
            candidates = candidates.len(),
            edited,
            companions = candidates.len() - edited,
            "cargo joint widen seed"
        );
        let lock_path = probe.project.root.join("Cargo.lock");
        let lock_text = std::fs::read_to_string(&lock_path)?;
        let before = CargoLock::parse(&lock_text)?;
        let mut identities = BTreeSet::new();
        let moves: Vec<_> = candidates
            .iter()
            .filter(|change| {
                current_selector(&before, change).is_some()
                    && !before.has_crates_io_package(&change.package.name, change.to.as_str())
                    && identities.insert(rejection_key(change))
            })
            .map(|change| PlannedNodeMove {
                name: change.package.name.clone(),
                from: change.from.to_string(),
                to: change.to.to_string(),
            })
            .collect();
        if !moves.is_empty() {
            let Some(seeded) = rewrite_planned_nodes(&lock_text, &moves) else {
                return Ok(GroupResolve::Discarded);
            };
            probe.journal.validate_project(&probe.project.root)?;
            std::fs::write(&lock_path, seeded)?;
        }
        probe.journal.validate_project(&probe.project.root)?;
        if let Some(observer) = probe.observer {
            observer.resolver_started(candidates.first().copied());
        }
        // A metadata resolve keeps every locked version its requirements admit, so a blocker the
        // candidates conflict with is released by name first.
        if !released.is_empty() {
            let unlocked = self
                .commands()
                .update_unlocking(probe.project, released)
                .await;
            probe.journal.validate_project(&probe.project.root)?;
            match unlocked {
                Ok(()) => {}
                Err(err) if seed_resolver_rejection(&err) => {
                    return Ok(GroupResolve::Refused {
                        parties: rejection_parties(&err),
                        chain: rejection_chain(&err),
                    });
                }
                Err(err) => return Err(err),
            }
        }
        let resolved = self.commands().metadata(probe.project).await;
        // Topology failure forbids any rejected-path restoration through another writer's files.
        probe.journal.validate_project(&probe.project.root)?;
        let graph = match resolved {
            Ok(graph) => graph,
            Err(err) if seed_resolver_rejection(&err) => {
                return Ok(GroupResolve::Refused {
                    parties: rejection_parties(&err),
                    chain: rejection_chain(&err),
                });
            }
            Err(err) => return Err(err),
        };
        // No follower edit follows this call: the graph and lock describe the retained manifests.
        let lock = read_lock(probe.project)?;
        if seed_has_drive_by(&before, &lock, &moves) {
            tracing::debug!("cargo joint widen moved a seeded package to an unplanned version");
            return Ok(GroupResolve::Discarded);
        }
        if let Some(violated) = culprit_drop.violated(probe.violations(&baseline, &lock)) {
            return Ok(violated);
        }
        Ok(GroupResolve::Resolved(Box::new(WidenEvidence {
            versions: lock.crates_io_locked_versions(),
            graph: Some(graph),
        })))
    }

    /// Lands `change`, whose own step split a declared crate, together with the planned moves
    /// that heal the split, as one atomic step.
    ///
    /// The partners are the planned, unreached moves of the split crate itself and of the
    /// requirers still binding its old line; a step that splits again names further partners.
    /// Each round adds at least one planned change to the coalition, so the plan's size bounds
    /// the rounds.
    /// The step releases `released` too, the blockers `change`'s own step had to release.
    /// It is kept only when every partner reaches its exact target and the step keeps the
    /// [acceptance rule](acceptance); otherwise the files are restored byte for byte.
    async fn complete_split(
        &self,
        probe: &WidenProbe<'_>,
        change: &Change,
        heal: SplitHeal,
        released: &[PackageKey],
    ) -> Result<SplitCompletion> {
        let start = journaled_state(probe.project, probe.journal)?;
        let completion_key = |coalition: &[&Change]| CompletionKey {
            root: probe.project.root.clone(),
            state: start,
            coalition: coalition
                .iter()
                .map(|change| rejection_key(change))
                .collect(),
            protected: probe
                .protected
                .iter()
                .map(|change| rejection_key(change))
                .collect(),
            rewrite: probe.rewrite,
            released: released
                .iter()
                .map(|node| (node.name.clone(), node.version.clone()))
                .collect(),
        };
        let mut coalition = vec![change];
        let mut split = heal;
        let mut tried = Vec::new();
        for _ in 0..probe.plan.len() {
            let partners =
                split_partners(probe.plan, &read_lock(probe.project)?, &split, &coalition);
            if partners.is_empty() {
                break;
            }
            coalition.extend(partners);
            let key = completion_key(&coalition);
            if self.completion_failed(&key) {
                break;
            }
            tried.push(key);
            let owners: Vec<&Change> = coalition.iter().chain(probe.protected).copied().collect();
            let checkpoint = widen_checkpoint(probe.project, &owners, probe.followers)?;
            let outcome = self
                .resolve_widened_group(probe, &coalition, released, CulpritDrop::First)
                .await?;
            tracing::debug!(
                change = ?rejection_key(change),
                coalition = ?coalition.iter().map(|change| rejection_key(change)).collect::<Vec<_>>(),
                landed = matches!(outcome, GroupResolve::Resolved(_)),
                "cargo split completion attempt"
            );
            match outcome {
                GroupResolve::Resolved(evidence)
                    if coalition
                        .iter()
                        .chain(probe.protected)
                        .all(|change| evidence.reached(change)) =>
                {
                    return Ok(SplitCompletion::Landed(evidence));
                }
                GroupResolve::Violated { violation, .. } => {
                    restore_widen_checkpoint(probe.project, probe.journal, &checkpoint)?;
                    let Some(next) = violation.heal() else {
                        break;
                    };
                    split = next;
                }
                GroupResolve::Resolved(_)
                | GroupResolve::Refused { .. }
                | GroupResolve::Discarded => {
                    restore_widen_checkpoint(probe.project, probe.journal, &checkpoint)?;
                    break;
                }
            }
        }
        if coalition.len() == 1 {
            return Ok(SplitCompletion::Unhealable);
        }
        self.failed_completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(tried);
        Ok(SplitCompletion::Refused)
    }

    /// Whether a completion of the same coalition from the same files already failed in this
    /// resolve.
    fn completion_failed(&self, key: &CompletionKey) -> bool {
        self.failed_completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(key)
    }

    async fn tentative_widen_logged(
        &self,
        probe: WidenProbe<'_>,
        change: &Change,
        rejections: &mut PinRejections,
        memo_stats: &mut MemoStats,
    ) -> Result<TentativeWiden> {
        let changes = [change];
        let memo_attempt = MemoAttempt {
            project: probe.project,
            plan: probe.plan,
            operation: MemoOperation::Widen,
            metadata_mode: probe.metadata_mode(),
            changes: &changes,
            followers: probe.followers,
            journal: probe.journal,
        };
        let key = self.rejection_memo_key(&memo_attempt)?;
        let fingerprint = widen_fingerprint(probe.project, probe.journal)?;
        let recalled = self.recall_rejection(&memo_attempt, key.as_ref(), memo_stats)?;
        let memo_hit = recalled.is_some();
        let attempt = if let Some(recalled) = recalled {
            recalled.apply(rejections);
            Ok(TentativeWiden::Rejected)
        } else {
            let owners: Vec<_> = std::iter::once(change)
                .chain(probe.protected.iter().copied())
                .collect();
            let checkpoint = widen_checkpoint(probe.project, &owners, probe.followers)?;
            let mut pins = PinBatchStats::default();
            let result = self
                .tentative_widen(probe, change, &checkpoint, rejections, &mut pins)
                .await;
            memo_stats.include(&pins.memo);
            if matches!(&result, Ok(TentativeWiden::Rejected)) && pins.memoizable_rejection() {
                self.remember_rejection(
                    &memo_attempt,
                    key,
                    MemoRejection {
                        effects: pins.rejection_effects,
                        heals: pins.heals,
                    },
                )?;
            }
            result
        };
        tracing::debug!(
            change = ?rejection_key(change),
            members = ?change.members,
            manifest_hash = fingerprint.as_ref().map(|hash| hash.manifests.as_str()),
            lock_hash = fingerprint.as_ref().map(|hash| hash.lock.as_str()),
            memo_hit,
            outcome = match &attempt {
                Ok(TentativeWiden::Unchanged) => "no-op",
                Ok(TentativeWiden::Landed(_)) => "landed",
                Ok(TentativeWiden::Rejected) => "rejected",
                Err(_) => "error",
            },
            "cargo per-change widen attempt"
        );
        attempt
    }

    /// Refuses `change`'s widen, whose step broke the [acceptance rule](acceptance) with
    /// `violation`, unless the step lands together with the planned moves that heal its split.
    async fn settle_widen_violation(
        &self,
        probe: &WidenProbe<'_>,
        change: &Change,
        violation: &Violation,
        checkpoint: &ProjectMutationJournal,
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<TentativeWiden> {
        self.note_refusal_constraints(
            &probe.project.root,
            change,
            violation.planned_constraint(change).map(str::to_owned),
        );
        let effect = RejectionEffect {
            key: rejection_key(change),
            detail: violation.to_string(),
            overwrite: false,
        };
        MemoRejection {
            effects: vec![effect.clone()],
            heals: Vec::new(),
        }
        .apply(rejections);
        stats.rejection_effects.push(effect);
        restore_widen_checkpoint(probe.project, probe.journal, checkpoint)?;
        let heal = violation.heal();
        let completion = match heal.clone() {
            Some(heal) => self.complete_split(probe, change, heal, &[]).await?,
            None => SplitCompletion::Refused,
        };
        match (completion, heal) {
            (SplitCompletion::Landed(evidence), _) => {
                return Ok(TentativeWiden::Landed(evidence));
            }
            // A split refuses the step under every plan, and only a planned move can heal
            // it, so the refusal stands wherever the plan carries none.
            (SplitCompletion::Unhealable, Some(heal)) => {
                stats.record_rejection(true);
                stats.heals.push(heal);
            }
            // Any other verdict depends on the plan, which the memo key omits.
            (SplitCompletion::Unhealable | SplitCompletion::Refused, _) => {
                stats.memo_disqualified = true;
            }
        }
        Ok(TentativeWiden::Rejected)
    }

    async fn tentative_widen(
        &self,
        probe: WidenProbe<'_>,
        change: &Change,
        checkpoint: &ProjectMutationJournal,
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<TentativeWiden> {
        probe.journal.validate_project(&probe.project.root)?;
        let baseline =
            ResolveBaseline::capture(&read_lock(probe.project)?, &probe.project.generated_members);
        if widen_and_follow_checked(
            &probe.project.root,
            change,
            probe.followers,
            probe.rewrite,
            || probe.journal.validate_project(&probe.project.root),
        )?
        .modified
        .is_empty()
        {
            return Ok(TentativeWiden::Unchanged);
        }
        self.pin_batch_recorded(
            probe.scope(),
            std::slice::from_ref(change),
            rejections,
            stats,
        )
        .await?;
        // Metadata may land a fork that a precise pin could not express.
        // A candidate rejection retains Cargo's own explanation and restores the tentative widen.
        let landed_graph = if probe.metadata_mode() == MemoMetadataMode::Resolve {
            probe.journal.validate_project(&probe.project.root)?;
            let resolved = self
                .metadata_with_observer(probe.project, probe.observer)
                .await;
            probe.journal.validate_project(&probe.project.root)?;
            match resolved {
                Ok(graph) => Some(graph),
                Err(err) if err.is_tool_spawn_failure() || err.is_local_environment_failure() => {
                    stats.memo_disqualified = true;
                    return Err(err);
                }
                Err(err) => {
                    if let Some(summary) = summarize_pin_rejection(&err) {
                        let effect = RejectionEffect {
                            key: rejection_key(change),
                            detail: summary,
                            overwrite: false,
                        };
                        MemoRejection {
                            effects: vec![effect.clone()],
                            heals: Vec::new(),
                        }
                        .apply(rejections);
                        stats.rejection_effects.push(effect);
                    }
                    // Reach failure alone is not a resolver rejection eligible for memoization.
                    stats.record_rejection(memoizable_resolver_rejection(&err));
                    restore_widen_checkpoint(probe.project, probe.journal, checkpoint)?;
                    return Ok(TentativeWiden::Rejected);
                }
            }
        } else {
            None
        };
        let lock = read_lock(probe.project)?;
        if let Some(violation) = probe.violation(&baseline, &lock) {
            return self
                .settle_widen_violation(&probe, change, &violation, checkpoint, rejections, stats)
                .await;
        }
        let evidence = WidenEvidence {
            versions: lock.crates_io_locked_versions(),
            graph: landed_graph,
        };
        let protected_reached = probe
            .protected
            .iter()
            .all(|change| evidence.reached(change));
        if !protected_reached {
            // Displacement depends on protected obligations, not just the candidate's memo key.
            stats.memo_disqualified = true;
        }
        if evidence.reached(change) && protected_reached {
            Ok(TentativeWiden::Landed(Box::new(evidence)))
        } else {
            restore_widen_checkpoint(probe.project, probe.journal, checkpoint)?;
            Ok(TentativeWiden::Rejected)
        }
    }

    async fn pin_unwidened_protected(
        &self,
        probe: WidenProbe<'_>,
        changes: &[Change],
        rejections: &mut PinRejections,
        memo_stats: &mut MemoStats,
    ) -> Result<Option<WidenEvidence>> {
        probe.journal.validate_project(&probe.project.root)?;
        let owners: Vec<_> = changes
            .iter()
            .chain(probe.protected.iter().copied())
            .collect();
        let checkpoint = widen_checkpoint(probe.project, &owners, probe.followers)?;
        let baseline =
            ResolveBaseline::capture(&read_lock(probe.project)?, &probe.project.generated_members);
        self.pin_batch_with_memo_stats(probe.scope(), changes, rejections, memo_stats)
            .await?;
        let needs_graph = owners
            .iter()
            .chain(probe.candidates)
            .any(|change| needs_member_graph(change));
        let graph = match self
            .widen_round_graph(probe.project, needs_graph, probe.journal, probe.observer)
            .await
        {
            Ok(graph) => graph,
            Err(err) if seed_resolver_rejection(&err) => {
                restore_widen_checkpoint(probe.project, probe.journal, &checkpoint)?;
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        let lock = read_lock(probe.project)?;
        let violation = probe.violation(&baseline, &lock);
        let evidence = WidenEvidence {
            versions: lock.crates_io_locked_versions(),
            graph,
        };
        if violation.is_none()
            && probe
                .protected
                .iter()
                .all(|change| evidence.reached(change))
        {
            Ok(Some(evidence))
        } else {
            restore_widen_checkpoint(probe.project, probe.journal, &checkpoint)?;
            Ok(None)
        }
    }

    /// Makes the generated projections follow the moves the pin phase landed *beyond* the plan:
    /// a planned move drags its dependents' companions to other lines (`toml 0.7 → 0.8` takes
    /// `toml_edit 0.19 → 0.22`), and following only the planned change would leave the
    /// projection demanding the companion's old line — which cargo then keeps alive as a second
    /// copy for the projection's sake alone. Every crates.io node only the generated members
    /// reach now, and did not before the mutation, is such a copy; its projection follows to
    /// the line the authored graph resolves the crate to, and one re-resolve lets cargo drop the
    /// node. A crate the authored graph no longer resolves at all is simply a stale projection
    /// line, left for the regeneration the report asks for.
    ///
    /// Both reachabilities are read from lock text, the same model on both sides: the baseline
    /// from the journal's preimage — fixed for the mutation, so a trial's rollback cannot leave a
    /// later trial judging against the state the rolled-back trial reached — and the current
    /// state from the lock on disk. Cargo is spawned only to re-resolve after a follow. Bounded
    /// because a followed companion can have companions of its own; a pass that follows nothing
    /// ends it, and so does a re-resolve that breaks the [acceptance rule](acceptance), whose
    /// follows and lock are restored.
    async fn reconcile_projections(&self, scope: PinScope<'_>) -> Result<()> {
        let PinScope {
            project,
            plan,
            journal,
            observer,
        } = scope;
        let facts = self.generated_facts(project, observer).await?;
        if facts.members.is_empty() {
            return Ok(());
        }
        let (authored_ids, generated_ids) = (&facts.authored_identities, &facts.identities);
        let stale_before = journal_preimage_lock(journal)?
            .map(|lock| lock.reached_only_via(authored_ids, generated_ids))
            .unwrap_or_default();
        for _ in 0..4 {
            let lock = read_lock(project)?;
            let baseline = ResolveBaseline::capture(&lock, &project.generated_members);
            let snapshot = widen_snapshot(&project.root, &[], &facts.members)?;
            let authored = lock.authored_crates_io_versions(authored_ids, generated_ids);
            let mut followed = false;
            for (name, retained) in lock.reached_only_via(authored_ids, generated_ids) {
                if stale_before.contains(&(name.clone(), retained.clone())) {
                    continue;
                }
                let Some(target) = authored
                    .get(&name)
                    .and_then(|versions| companion_target(versions, &retained))
                else {
                    continue;
                };
                journal.validate_project(&project.root)?;
                followed |= !manifest::follow_constraint(
                    &project.root,
                    &facts.members,
                    &name,
                    &retained,
                    target,
                )?
                .modified
                .is_empty();
            }
            if !followed {
                return Ok(());
            }
            // The re-resolve that lets cargo drop the node the projection no longer demands.
            journal.validate_project(&project.root)?;
            if let Some(observer) = observer {
                observer.resolver_started(None);
            }
            self.commands().resolve(project).await?;
            journal.validate_project(&project.root)?;
            let after = read_lock(project)?;
            if let Some(violation) = baseline.violation(&after, &project.generated_members, plan) {
                tracing::debug!(
                    %violation,
                    "restoring a projection follow that broke the acceptance rule"
                );
                restore_widen_snapshot(&project.root, &snapshot, journal)?;
                return Ok(());
            }
        }
        Ok(())
    }

    /// Lands the co-planned changes the per-package pin passes could not: a family whose members'
    /// requirements only admit each other's *source* era (icu's `~2.3.0` tilde web between
    /// libraries and their data crates) rejects every single-package `--precise` step no matter
    /// the order — each step leaves a sibling's requirement unsatisfied. Seeding the staged lock
    /// with every still-short rejected change at its target and running one unlocked resolve
    /// reconciles the family as a unit: the resolver refills each seeded node's dependencies and
    /// checksum from its real manifest, resolves era-mates the seeds newly require, and drops
    /// nodes only the source era referenced.
    ///
    /// The attempt is strictly additive to the pin passes and all-or-nothing. It runs only over
    /// changes already rejected and still short of their target, needs at least two of them (a
    /// lone rejection was already expressed exactly by its pin), abstains rather than seed a
    /// partial group, and restores the pre-attempt lock byte-for-byte unless *every* seeded
    /// member landed at its exact target — a partially-landed reconcile left some member's
    /// unanchored slot to the live index's preference, and keeping that would be a silent
    /// drive-by move. The seed is restored, too, when the reconcile breaks the
    /// [acceptance rule](acceptance). A restored group's members keep their recorded per-pin
    /// rejections.
    async fn land_rejected_group_atomically(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
        rejections: &mut PinRejections,
        memo_stats: &mut MemoStats,
    ) -> Result<()> {
        if rejections.is_empty() {
            return Ok(());
        }
        let lock = read_lock(project)?;
        let mut moves: Vec<PlannedNodeMove> = Vec::new();
        let mut keys: Vec<(String, String, String)> = Vec::new();
        for change in &plan.changes {
            let key = rejection_key(change);
            // A stale rejection whose change a later pin pass still landed keeps its map entry;
            // membership here is decided by the lock, not the map.
            if !rejections.contains_key(&key)
                || keys.contains(&key)
                || !lock.has_crates_io_package(&change.package.name, change.from.as_str())
                || lock.has_crates_io_package(&change.package.name, change.to.as_str())
            {
                continue;
            }
            moves.push(PlannedNodeMove {
                name: change.package.name.clone(),
                from: change.from.as_str().to_string(),
                to: change.to.as_str().to_string(),
            });
            keys.push(key);
        }
        if moves.len() < 2 {
            return Ok(());
        }
        tracing::debug!(
            group = moves.len(),
            "seeding rejected co-planned changes for one atomic reconcile"
        );
        let mut stats = PinBatchStats::default();
        let obligations: Vec<_> = plan
            .changes
            .iter()
            .filter(|change| keys.contains(&rejection_key(change)))
            .collect();
        let seeded = self
            .seed_resolve(
                project,
                SeedGroup {
                    moves: &moves,
                    changes: &obligations,
                    plan: &plan.changes,
                },
                journal,
                observer.zip(
                    plan.changes
                        .iter()
                        .find(|change| keys.contains(&rejection_key(change))),
                ),
                &mut stats,
            )
            .await;
        memo_stats.include(&stats.memo);
        let Some(seeded) = seeded?.into_landed() else {
            return Ok(());
        };
        // All-or-nothing: the seed removed every `from` node, so a member the reconcile did NOT
        // land at its target has an unanchored slot the resolver filled from the live index —
        // possibly with a version fresher than the member started at, which later rounds cannot
        // always mature back down (the only in-range candidate may be the still-fresh original).
        // Keeping a partial landing would trade the group's progress for a silent drive-by
        // upgrade; restoring keeps the spec's "verify every planned target landed" contract, and
        // each member then still holds the per-pin rejection cargo explained.
        let all_landed = moves.iter().all(|planned| {
            seeded
                .after
                .has_crates_io_package(&planned.name, &planned.to)
        });
        if all_landed {
            for key in &keys {
                rejections.remove(key);
            }
        } else {
            restore_widen_snapshot(&project.root, &seeded.snapshot, journal)?;
        }
        Ok(())
    }

    /// Seeds one group and restores its snapshot on resolver or environment failure, or when the
    /// resolve breaks the [acceptance rule](acceptance) against the lock before the seed.
    async fn seed_resolve(
        &self,
        project: &Project,
        group: SeedGroup<'_>,
        journal: &ProjectMutationJournal,
        notification: Option<(&dyn ApplyObserver, &Change)>,
        stats: &mut PinBatchStats,
    ) -> Result<SeedOutcome> {
        let moves = group.moves;
        let lock_path = project.root.join("Cargo.lock");
        let lock_text = std::fs::read_to_string(&lock_path)?;
        let before = CargoLock::parse(&lock_text)?;
        let baseline = ResolveBaseline::capture(&before, &project.generated_members);
        let Some(seeded) = rewrite_planned_nodes(&lock_text, moves) else {
            return Ok(SeedOutcome::Refused);
        };
        // The generated projections follow the seeded moves as they follow a pin: a projection
        // still demanding a seeded node's old version would reject the joint resolve for the
        // projection's sake.
        // Captured with the lock, restored with it.
        let followers = self.pin_followers(project).await?;
        let memo_attempt = MemoAttempt {
            project,
            plan: group.plan,
            operation: MemoOperation::Seed,
            metadata_mode: MemoMetadataMode::Resolve,
            changes: group.changes,
            followers: &followers,
            journal,
        };
        let key = self.rejection_memo_key(&memo_attempt)?;
        if self
            .recall_rejection(&memo_attempt, key.as_ref(), &mut stats.memo)?
            .is_some()
        {
            stats.resolver_rejected = true;
            return Ok(SeedOutcome::Refused);
        }
        let snapshot = widen_snapshot(&project.root, &[], &followers)?;
        journal.validate_project(&project.root)?;
        for planned in moves {
            manifest::follow_constraint_checked(
                &project.root,
                &followers,
                &planned.name,
                &planned.from,
                &planned.to,
                || journal.validate_project(&project.root),
            )?;
        }
        journal.validate_project(&project.root)?;
        std::fs::write(&lock_path, &seeded)?;
        stats.seeded += moves.len();
        stats.resolves += 1;
        if let Some((observer, change)) = notification {
            observer.resolver_started(Some(change));
        }
        let resolved = self.commands().resolve(project).await;
        // A coordination failure forbids restoring files that may now belong to another writer.
        journal.validate_project(&project.root)?;
        match resolved {
            Ok(()) => {
                let after = read_lock(project)?;
                if let Some(violation) =
                    baseline.violation(&after, &project.generated_members, group.plan)
                {
                    tracing::debug!(
                        group = moves.len(),
                        %violation,
                        "restoring a cargo seed that broke the acceptance rule"
                    );
                    // The verdict depends on the plan, which the memo key omits.
                    stats.memo_disqualified = true;
                    restore_widen_snapshot(&project.root, &snapshot, journal)?;
                    return Ok(SeedOutcome::Violated(violation));
                }
                replay_seeded_follows(&project.root, &snapshot, &followers, moves, &after)?;
                Ok(SeedOutcome::Landed(SeededResolve {
                    before,
                    after,
                    snapshot,
                }))
            }
            Err(err) => {
                stats.record_rejection(memoizable_resolver_rejection(&err));
                restore_widen_snapshot(&project.root, &snapshot, journal)?;
                if seed_resolver_rejection(&err) {
                    if stats.memoizable_rejection() {
                        self.remember_rejection(&memo_attempt, key, MemoRejection::default())?;
                    }
                    // A group rejection cannot explain any one candidate's refusal.
                    Ok(SeedOutcome::Refused)
                } else {
                    Err(err)
                }
            }
        }
    }

    /// Applies all `changes` as one logical unit, driving each to its exact target.
    ///
    /// A seed expresses all targets beside one unlocked resolve, avoiding one process per crate.
    /// Seeded slots must stay at their original or requested version; other movement is reported.
    /// Failed groups split deterministically until a single change needs the precise-pin loop.
    /// Successful seeds send only their unlanded changes to that loop.
    /// Resolver rejections remain non-fatal held candidates the final diff reports; local
    /// environment failures and interruptions abort without bisection.
    #[cfg(all(test, unix))]
    async fn pin_batch(
        &self,
        project: &Project,
        changes: &[Change],
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
        rejections: &mut PinRejections,
    ) -> Result<()> {
        let scope = PinScope {
            project,
            plan: changes,
            journal,
            observer,
        };
        self.pin_batch_recorded(scope, changes, rejections, &mut PinBatchStats::default())
            .await
    }

    async fn pin_batch_recorded(
        &self,
        scope: PinScope<'_>,
        changes: &[Change],
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<()> {
        // Direct workspace members can emit sibling changes sharing `(package, from, to)`;
        // those are one lock move and one resolver operation.
        let mut sorted = changes.to_vec();
        sorted.sort_by(|a, b| {
            a.package
                .name
                .cmp(&b.package.name)
                .then_with(|| a.package.registry.cmp(&b.package.registry))
                .then_with(|| a.from.as_str().cmp(b.from.as_str()))
                .then_with(|| a.to.as_str().cmp(b.to.as_str()))
        });
        let mut obligations: Vec<Change> = Vec::new();
        for change in sorted {
            if let Some(previous) = obligations.last_mut()
                && previous.package == change.package
                && previous.from == change.from
                && previous.to == change.to
            {
                previous.members.extend(change.members);
                previous
                    .members
                    .sort_by(|a, b| (&a.name, &a.path).cmp(&(&b.name, &b.path)));
                previous.members.dedup();
                previous.direct |= change.direct;
            } else {
                obligations.push(change);
            }
        }
        let worklist: Vec<_> = obligations.iter().collect();
        let result = self
            .pin_fixed_point(scope, &worklist, rejections, stats)
            .await;
        tracing::debug!(
            resolves = stats.resolves,
            seeded = stats.seeded,
            landed = stats.landed,
            bisected = stats.bisected,
            per_crate = stats.per_crate,
            memo_hits = stats.memo.hits,
            memo_misses = stats.memo.misses,
            "cargo pin batch finished"
        );
        result
    }

    async fn pin_batch_with_memo_stats(
        &self,
        scope: PinScope<'_>,
        changes: &[Change],
        rejections: &mut PinRejections,
        memo_stats: &mut MemoStats,
    ) -> Result<()> {
        let mut stats = PinBatchStats::default();
        let result = self
            .pin_batch_recorded(scope, changes, rejections, &mut stats)
            .await;
        memo_stats.include(&stats.memo);
        result
    }

    async fn pin_fixed_point(
        &self,
        scope: PinScope<'_>,
        worklist: &[&Change],
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<()> {
        // A target the first pass left unlanded can land once a later pin moved what held it.
        // Revisit only targets still needing a pin, retaining the original cycle and pass bounds.
        let mut seen = BTreeSet::new();
        for _ in 0..worklist.len().saturating_add(1) {
            let before = read_lock(scope.project)?.locked_slots();
            if !seen.insert(before.clone()) {
                break;
            }
            self.pin_seeded_worklist(scope, worklist, rejections, stats)
                .await?;
            if read_lock(scope.project)?.locked_slots() == before {
                break;
            }
        }
        Ok(())
    }

    async fn pin_seeded_worklist(
        &self,
        scope: PinScope<'_>,
        worklist: &[&Change],
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<()> {
        let PinScope {
            project,
            plan,
            journal,
            observer,
        } = scope;
        let pass = PinPass {
            scope,
            batch: worklist,
        };
        let mut groups = vec![worklist.to_vec()];
        while let Some(group) = groups.pop() {
            let lock = read_lock(project)?;
            let (refused, mut unlanded): (Vec<&Change>, Vec<&Change>) = group
                .iter()
                .copied()
                .filter(|change| current_selector(&lock, change).is_some())
                .partition(|change| self.was_refused_alone(change));
            if !refused.is_empty() {
                self.pin_individually(pass, &refused, rejections, stats)
                    .await?;
            }
            if unlanded.is_empty() {
                continue;
            }
            let lock = read_lock(project)?;
            let moves = seed_moves(&lock, &unlanded);
            // Seeding an already present target would create duplicate lock blocks, and a precise
            // pin can still converge the original source copy into that target.
            // A lone change gains nothing from a seed either: the precise pin is one resolve too,
            // and only it records cargo's explanation when the change is refused.
            if let [lone] = unlanded.as_slice() {
                self.pin_alone(pass, lone, rejections, stats).await?;
                continue;
            }
            if moves.is_empty() {
                self.pin_individually(pass, &unlanded, rejections, stats)
                    .await?;
                continue;
            }
            let notification = observer.zip(unlanded.first().copied());
            let seeded = match self
                .seed_resolve(
                    project,
                    SeedGroup {
                        moves: &moves,
                        changes: &unlanded,
                        plan,
                    },
                    journal,
                    notification,
                    stats,
                )
                .await?
            {
                SeedOutcome::Landed(seeded) => Some(seeded),
                // The changes the violation names go on alone, and the rest seed again together,
                // instead of halving a large group until the culprit is alone.
                SeedOutcome::Violated(violation) => {
                    if let Some(CulpritSplit { named, rest }) = violation.split_culprits(&unlanded)
                    {
                        stats.bisected += 1;
                        groups.push(rest);
                        groups.extend(named.into_iter().map(|change| vec![change]));
                        continue;
                    }
                    None
                }
                SeedOutcome::Refused => None,
            };
            if let Some(seeded) = seeded {
                if seed_has_drive_by(&seeded.before, &seeded.after, &moves) {
                    tracing::debug!(group = moves.len(), "restoring cargo seed after a drive-by");
                    restore_widen_snapshot(&project.root, &seeded.snapshot, journal)?;
                } else {
                    let leftovers: Vec<&Change> = unlanded
                        .iter()
                        .copied()
                        .filter(|change| current_selector(&seeded.after, change).is_some())
                        .collect();
                    stats.landed += unlanded.len() - leftovers.len();
                    self.pin_individually(pass, &leftovers, rejections, stats)
                        .await?;
                    continue;
                }
            }
            if let [lone] = unlanded.as_slice() {
                self.pin_alone(pass, lone, rejections, stats).await?;
            } else {
                stats.bisected += 1;
                let right = unlanded.split_off(unlanded.len() / 2);
                groups.push(right);
                groups.push(unlanded);
            }
        }
        Ok(())
    }

    /// Pins one change precisely and remembers it when it does not land, so later batches keep it
    /// out of their seeds.
    async fn pin_alone(
        &self,
        pass: PinPass<'_>,
        change: &Change,
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<()> {
        self.pin_individually(pass, &[change], rejections, stats)
            .await?;
        if current_selector(&read_lock(pass.scope.project)?, change).is_some() {
            self.refused_alone
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(rejection_key(change));
        }
        Ok(())
    }

    fn was_refused_alone(&self, change: &Change) -> bool {
        self.refused_alone
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&rejection_key(change))
    }

    /// Preserves the precise-pin fixed point for failed leaves and successful-seed leftovers.
    ///
    /// Each command can move or remove another planned node, so every pass re-reads the lock and
    /// uses [`current_selector`] to address the original or unique off-target version.
    /// A pin Cargo refuses because another locked node stands in the way is retried with that node
    /// unlocked (see [`pin_unlocking_blockers`](Self::pin_unlocking_blockers)).
    async fn pin_individually(
        &self,
        pass: PinPass<'_>,
        worklist: &[&Change],
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<()> {
        let PinScope {
            project,
            journal,
            observer,
            ..
        } = pass.scope;
        let mut seen = BTreeSet::new();
        for _ in 0..worklist.len().saturating_add(1) {
            let before = read_lock(project)?.locked_slots();
            if !seen.insert(before.clone()) {
                break;
            }
            let mut attempted = false;
            for change in worklist {
                let lock = read_lock(project)?;
                let Some(current) = current_selector(&lock, change) else {
                    continue;
                };
                attempted = true;
                journal.validate_project(&project.root)?;
                let followers = self.pin_followers(project).await?;
                let changes = [*change];
                let memo_attempt = MemoAttempt {
                    project,
                    plan: pass.scope.plan,
                    operation: MemoOperation::Precise,
                    metadata_mode: MemoMetadataMode::Skip,
                    changes: &changes,
                    followers: &followers,
                    journal,
                };
                let key = self.rejection_memo_key(&memo_attempt)?;
                let fingerprint = widen_fingerprint(project, journal)?;
                if let Some(rejection) =
                    self.recall_rejection(&memo_attempt, key.as_ref(), &mut stats.memo)?
                {
                    stats.record_effects(&rejection, rejections);
                    stats.resolver_rejected = true;
                    log_pin_attempt(change, fingerprint.as_ref(), true, "rejected");
                    continue;
                }
                if let Some(observer) = observer {
                    observer.resolver_started(Some(change));
                }
                stats.resolves += 1;
                stats.per_crate += 1;
                let updated = self.update_precise(pass.scope, change, &current).await;
                let validated = journal.validate_project(&project.root);
                stats.memo_disqualified |= updated.is_err() || validated.is_err();
                let outcome = match (&validated, &updated) {
                    (Err(_), _) => "topology-error",
                    (Ok(()), Ok(PrecisePinOutcome::Rejected { .. })) => "rejected",
                    (Ok(()), Ok(PrecisePinOutcome::Completed)) => "completed",
                    (Ok(()), Err(_)) => "error",
                };
                log_pin_attempt(change, fingerprint.as_ref(), false, outcome);
                validated?;
                if let PrecisePinOutcome::Rejected {
                    detail,
                    resolver_rejection,
                    parties,
                    split,
                } = updated?
                {
                    let refused = RefusedPin {
                        change,
                        current: &current,
                        detail,
                        resolver_rejection,
                        parties,
                        split,
                    };
                    self.settle_refused_pin(pass, refused, &memo_attempt, key, rejections, stats)
                        .await?;
                }
            }
            let after = read_lock(project)?.locked_slots();
            if !attempted || after == before {
                break;
            }
        }
        Ok(())
    }

    /// Lands a refused pin by releasing its blockers, or records Cargo's refusal for the candidate's
    /// held row and, where the outcome is reusable, in the rejection memo.
    async fn settle_refused_pin(
        &self,
        pass: PinPass<'_>,
        refused: RefusedPin<'_>,
        memo_attempt: &MemoAttempt<'_>,
        key: Option<MemoKey>,
        rejections: &mut PinRejections,
        stats: &mut PinBatchStats,
    ) -> Result<()> {
        let unlocking = self.pin_unlocking_blockers(pass, &refused, stats).await?;
        let split = match unlocking {
            Unlocking::Landed => return Ok(()),
            Unlocking::Refused(Some(split)) => Some(split),
            Unlocking::Refused(None) | Unlocking::NothingToRelease => {
                refused.split.clone().map(|heal| UnlockedSplit {
                    heal,
                    released: Vec::new(),
                })
            }
        };
        let mut unhealable = None;
        if let Some(UnlockedSplit { heal, released }) = split {
            let PinScope {
                project,
                plan,
                journal,
                observer,
            } = pass.scope;
            let followers = self.pin_followers(project).await?;
            // Requirements a split can heal through are widened already under `Always`, and are
            // widened here only where they do not admit the target.
            let probe = WidenProbe {
                project,
                plan,
                rewrite: RewriteMode::Auto,
                candidates: &[],
                protected: &[],
                followers: &followers,
                journal,
                observer,
            };
            match self
                .complete_split(&probe, refused.change, heal.clone(), &released)
                .await?
            {
                SplitCompletion::Landed(_) => return Ok(()),
                SplitCompletion::Unhealable if released.is_empty() => unhealable = Some(heal),
                SplitCompletion::Unhealable | SplitCompletion::Refused => {}
            }
        }
        let RefusedPin {
            change,
            detail,
            resolver_rejection,
            ..
        } = refused;
        let effects: Vec<_> = detail
            .into_iter()
            .map(|detail| RejectionEffect {
                key: rejection_key(change),
                detail,
                overwrite: true,
            })
            .collect();
        // A split the pin's own landing caused refuses it under every plan, and only a planned
        // move can heal it, so the refusal stands wherever the plan carries none.
        let memoizable = resolver_rejection || unhealable.is_some();
        let rejection = MemoRejection {
            effects,
            heals: unhealable.into_iter().collect(),
        };
        // Last rejection wins; a candidate a later pass still lands never reads its
        // stale entry (details are consulted only for unreached candidates).
        stats.record_effects(&rejection, rejections);
        stats.record_rejection(memoizable);
        if stats.memoizable_rejection() && memoizable {
            self.remember_rejection(memo_attempt, key, rejection)?;
        }
        Ok(())
    }

    /// Starts a resolve of `root` with no refusal constraints or failed split completions
    /// recorded, since both depend on the plan.
    fn forget_resolve_state(&self, root: &Utf8Path) {
        self.refusal_constraints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(constrained, _), _| constrained != root);
        self.failed_completions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|completion| completion.root != root);
    }

    /// Records the other planned crates `change`'s refusal in `root` depended on.
    fn note_refusal_constraints(
        &self,
        root: &Utf8Path,
        change: &Change,
        names: impl IntoIterator<Item = String>,
    ) {
        let mut names = names.into_iter().peekable();
        if names.peek().is_none() {
            return;
        }
        self.refusal_constraints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry((root.to_path_buf(), rejection_key(change)))
            .or_default()
            .extend(names);
    }

    /// Retries a refused precise pin with the locked nodes Cargo blamed released beside it, so a
    /// target that needs another crate's version moved (pest 2.9.3 pins `psm = "=0.1.24"` while
    /// another dependent holds psm 0.1.32) still lands within the requirement that admits it,
    /// instead of being held or forcing a raised floor into a manifest other projects resolve.
    ///
    /// A retry Cargo also refuses names the next blockers, and a bounded number of rounds release
    /// them too.
    /// A landing is kept only when the candidate reached its exact target and the step keeps the
    /// [acceptance rule](acceptance); otherwise the lock is restored byte for byte.
    /// The released nodes may move anywhere their requirements admit: the apply reports them as
    /// collateral rows, and the executor's transitive gate matures any that floated too fresh.
    /// Returns whether the pin landed, or that the refusal named nothing to release.
    async fn pin_unlocking_blockers(
        &self,
        pass: PinPass<'_>,
        refused: &RefusedPin<'_>,
        stats: &mut PinBatchStats,
    ) -> Result<Unlocking> {
        let PinPass { scope, batch } = pass;
        let PinScope {
            project,
            journal,
            observer,
            ..
        } = scope;
        let RefusedPin {
            change, current, ..
        } = *refused;
        let before_text = std::fs::read_to_string(project.root.join("Cargo.lock"))?;
        let before = CargoLock::parse(&before_text)?;
        // Which conflict parties may be released depends on the batch, which the memo key omits,
        // so a refusal naming any is not remembered even when this batch releases none of them.
        if !conflicting_nodes(&before, change, &refused.parties).is_empty() {
            stats.memo_disqualified = true;
        }
        let baseline = ResolveBaseline::capture(&before, &project.generated_members);
        self.note_refusal_constraints(
            &project.root,
            change,
            withheld_planned(&before, change, batch, &refused.parties),
        );
        let mut blockers = pin_blockers(&before, &baseline, change, batch, &refused.parties);
        if blockers.is_empty() {
            return Ok(Unlocking::NothingToRelease);
        }
        let mut released: Vec<PackageKey> = Vec::new();
        for _ in 0..PIN_BLOCKER_ROUNDS {
            blockers.retain(|node| !released.contains(node));
            if blockers.is_empty() {
                break;
            }
            released.append(&mut blockers);
            journal.validate_project(&project.root)?;
            if let Some(observer) = observer {
                observer.resolver_started(Some(change));
            }
            stats.resolves += 1;
            stats.per_crate += 1;
            let updated = self
                .commands()
                .update_precise_crates_io_unlocking(
                    project,
                    &change.package.name,
                    current,
                    change.to.as_str(),
                    &released,
                )
                .await;
            journal.validate_project(&project.root)?;
            match updated {
                Ok(()) => {
                    let unlocked = UnlockedPin {
                        change,
                        baseline: &baseline,
                        before_text: &before_text,
                        released: &released,
                    };
                    return self.judge_unlocked_pin(scope, &unlocked);
                }
                Err(err) if err.is_local_environment_failure() => return Err(err),
                Err(err) => {
                    let parties = rejection_parties(&err);
                    self.note_refusal_constraints(
                        &project.root,
                        change,
                        withheld_planned(&before, change, batch, &parties),
                    );
                    blockers = pin_blockers(&before, &baseline, change, batch, &parties);
                }
            }
        }
        tracing::debug!(
            change = ?rejection_key(change),
            released = ?released,
            "cargo unlocking pin refused"
        );
        Ok(Unlocking::Refused(None))
    }

    /// Keeps an unlocking pin that reached its target within the [acceptance rule](acceptance),
    /// and otherwise restores the lock it started from.
    fn judge_unlocked_pin(
        &self,
        scope: PinScope<'_>,
        unlocked: &UnlockedPin<'_>,
    ) -> Result<Unlocking> {
        let PinScope {
            project,
            plan,
            journal,
            ..
        } = scope;
        let UnlockedPin {
            change,
            baseline,
            before_text,
            released,
        } = *unlocked;
        let after = read_lock(project)?;
        let violation = baseline.violation(&after, &project.generated_members, plan);
        let kept = reached(&after.crates_io_locked_versions(), change) && violation.is_none();
        tracing::debug!(
            change = ?rejection_key(change),
            released = ?released,
            kept,
            violation = violation.as_ref().map(ToString::to_string),
            "cargo unlocking pin attempt"
        );
        if kept {
            return Ok(Unlocking::Landed);
        }
        self.note_refusal_constraints(
            &project.root,
            change,
            violation
                .as_ref()
                .and_then(|violation| violation.planned_constraint(change))
                .map(str::to_owned),
        );
        journal.validate_project(&project.root)?;
        std::fs::write(project.root.join("Cargo.lock"), before_text)?;
        Ok(Unlocking::Refused(
            violation
                .as_ref()
                .and_then(Violation::heal)
                .map(|heal| UnlockedSplit {
                    heal,
                    released: released.to_vec(),
                }),
        ))
    }

    /// Issues one tolerant precise pin, separating resolver rejection from local breakage.
    ///
    /// A rejected precise pin is a resolver outcome the final lock diff reports as held; the
    /// returned summary of cargo's own explanation lets the skip row name the blocking requirement
    /// instead of the generic "the resolver rejected this change". Broken local state must
    /// propagate; otherwise disk-full or spawn failures would masquerade as a conflict in the
    /// candidate set.
    ///
    /// A pin that carries its own candidate past the target is re-pinned to the target at once:
    /// once a widened requirement orphans the source node, Cargo resolves the new line freshly,
    /// to its newest version.
    /// The pin, with any re-pin, is still refused, its lock restored byte for byte, when the move
    /// it made breaks the [acceptance rule](acceptance): Cargo floats a dependency of the target to
    /// the newest version its requirements admit, which can be a directly declared crate or a
    /// planned crate past its own target.
    async fn update_precise(
        &self,
        scope: PinScope<'_>,
        change: &Change,
        current: &str,
    ) -> Result<PrecisePinOutcome> {
        let PinScope {
            project,
            plan,
            journal,
            ..
        } = scope;
        let lock_path = project.root.join("Cargo.lock");
        let before_text = std::fs::read_to_string(&lock_path)?;
        let generated = &project.generated_members;
        let baseline = ResolveBaseline::capture(&CargoLock::parse(&before_text)?, generated);
        let pinned = match self
            .commands()
            .update_precise_crates_io(project, &change.package.name, current, change.to.as_str())
            .await
        {
            Ok(()) => self.repin_overshoot(scope, change).await,
            Err(err) => Err(err),
        };
        let restore = || -> Result<()> {
            journal.validate_project(&project.root)?;
            if std::fs::read_to_string(&lock_path)? != before_text {
                std::fs::write(&lock_path, &before_text)?;
            }
            Ok(())
        };
        match pinned {
            Ok(()) => {
                journal.validate_project(&project.root)?;
                let Some(violation) = baseline.violation(&read_lock(project)?, generated, plan)
                else {
                    return Ok(PrecisePinOutcome::Completed);
                };
                tracing::debug!(
                    change = ?rejection_key(change),
                    %violation,
                    "restoring a cargo pin that broke the acceptance rule"
                );
                self.note_refusal_constraints(
                    &project.root,
                    change,
                    violation.planned_constraint(change).map(str::to_owned),
                );
                restore()?;
                Ok(PrecisePinOutcome::Rejected {
                    detail: Some(violation.to_string()),
                    // The verdict depends on the plan, which the memo key omits: a split may land
                    // beside the planned moves that heal it.
                    resolver_rejection: false,
                    parties: Vec::new(),
                    split: violation.heal(),
                })
            }
            Err(err) if err.is_local_environment_failure() => Err(err),
            Err(err) => {
                // A refused re-pin leaves the overshoot behind; a refused first pin left nothing.
                restore()?;
                Ok(PrecisePinOutcome::Rejected {
                    resolver_rejection: memoizable_resolver_rejection(&err),
                    detail: summarize_pin_rejection(&err),
                    parties: rejection_parties(&err),
                    split: None,
                })
            }
        }
    }

    /// Re-pins `change` to its target when the pin just run left it on another version of its
    /// lines, so the step lands exactly or not at all.
    async fn repin_overshoot(&self, scope: PinScope<'_>, change: &Change) -> Result<()> {
        let PinScope {
            project,
            journal,
            observer,
            ..
        } = scope;
        journal.validate_project(&project.root)?;
        let Some(overshoot) = current_selector(&read_lock(project)?, change)
            .filter(|version| version != change.from.as_str())
        else {
            return Ok(());
        };
        tracing::debug!(
            change = ?rejection_key(change),
            overshoot,
            "re-pinning a cargo pin that overshot its target"
        );
        if let Some(observer) = observer {
            observer.resolver_started(Some(change));
        }
        self.commands()
            .update_precise_crates_io(
                project,
                &change.package.name,
                &overshoot,
                change.to.as_str(),
            )
            .await
    }

    async fn apply_plan(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<ApplyReport> {
        journal.validate_project(&project.root)?;
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }
        // The pre-apply lock, taken from the journal (`mutation_journal` captured `Cargo.lock` before
        // the re-resolve), parsed once: its slot map feeds the version diff below and its edge view
        // feeds the edge policy.
        // The batched precise pins emit one consistent lock; the report is the paired version-slot
        // diff of this snapshot against the result.
        // A missing or unparsable snapshot leaves `before` empty.
        // Planned target reporting still uses the resolved result, but collateral comparison then
        // has no baseline.
        let before_lock = journaled_lock(journal);

        // The whole graph is re-resolved as one logical batch, trying seeded targets before the
        // precise-pin fixed point for failed leaves and unlanded targets.
        // The diff below surfaces every paired version-slot move.
        let pin_phase = std::time::Instant::now();
        let WholeGraphResolve {
            rejections: pin_rejections,
            unmoved,
        } = match self
            .resolve_whole_graph(project, plan, journal, observer)
            .await
        {
            Ok(resolved) => resolved,
            Err(err) if err.is_tool_spawn_failure() => return Err(err),
            // The joint resolve is unsatisfiable as a whole (a `=`-pin conflict or an unfetchable
            // version). Propagate so the caller's `apply_resilient` can isolate the offending
            // candidate(s) and apply the rest, instead of holding every candidate.
            // Local environment failures propagate through `apply_resilient` without bisection.
            // The caller restores the journal, so no partial lock is kept.
            Err(err) => return Err(err),
        };

        tracing::debug!(
            candidates = plan.changes.len(),
            elapsed_ms = pin_phase.elapsed().as_millis(),
            "cargo pin phase finished"
        );
        let resolver_lock = read_lock(project)?;

        // The resolved graph supplies declared requirements to edge enforcement, proves which
        // direct member edges reached their targets, and names the requirement blocking a target
        // absent from its lock slot.
        // Preserve needs requirements only when the before/resolver lock pair contains an
        // addressable rebind; successful lock verification returns a fresh graph after any
        // correction.
        let resolver_versions = resolver_lock.crates_io_locked_versions();
        let needs_graph = needs_apply_graph(&plan.changes, &resolver_versions);
        let preserve_needs_graph = matches!(plan.edge_policy, EdgePolicy::Preserve)
            && before_lock.as_ref().is_some_and(|before_lock| {
                edges::preserve::has_potential_restoration(
                    &edges::LockEdgeView::from_lock(before_lock),
                    &edges::LockEdgeView::from_lock(&resolver_lock),
                )
            });
        let corrective_edges =
            matches!(plan.edge_policy, EdgePolicy::Canonicalize) || preserve_needs_graph;
        let graph = if needs_graph || corrective_edges {
            let scope = PinScope {
                project,
                plan: &plan.changes,
                journal,
                observer,
            };
            Some(self.judged_graph(scope, &resolver_lock).await?)
        } else {
            None
        };

        // Enforce the plan's edge policy over the re-resolved lock and collect the moves observation
        // can pair across stable dependent identities and coexisting endpoints.
        journal.validate_project(&project.root)?;
        let edge_phase = std::time::Instant::now();
        let generated = self.generated_facts(project, observer).await?;
        let enforced = edges::enforce::enforce(
            self.locked_reader(),
            project,
            plan.edge_policy,
            before_lock.as_ref(),
            graph,
            &generated.identities,
            observer,
        )
        .await?;
        tracing::debug!(
            elapsed_ms = edge_phase.elapsed().as_millis(),
            "cargo edge phase finished"
        );
        let graph = enforced.graph;
        let edge_rebinds = enforced.rebinds;

        let after_lock = read_lock(project)?;
        journal.validate_project(&project.root)?;
        let graph = self
            .guard_after_resolve(
                project,
                before_lock.as_ref(),
                &after_lock,
                graph,
                plan,
                observer,
            )
            .await?;
        let crates_io_after = after_lock.crates_io_locked_versions();
        classify_planned_changes(
            plan,
            &crates_io_after,
            graph.as_ref(),
            &pin_rejections,
            &mut report,
        );
        report.independent_holds =
            self.independent_holds(project, plan, journal, unmoved.as_ref(), &report);

        add_collateral_changes(
            before_lock.as_ref(),
            &after_lock,
            &project.generated_members,
            &mut report,
        );
        report.edge_rebinds = edge_rebinds;
        let followers = self.generated_facts(project, observer).await?;
        report.followed_manifests = followed_manifests(project, &followers.members, journal)?;
        Ok(report)
    }

    /// The skipped changes of `report` its apply would skip again in a plan of exactly its applied
    /// planned changes plus that change (see [`ApplyReport::independent_holds`]).
    ///
    /// The resolve landed nothing beyond its warm start, so every refusal it met was met in that
    /// state, and the narrower plan starts from the same state when the landing memo returns it
    /// for that plan.
    /// Each lone step of a held change there meets the same refusal unless that refusal hinged on
    /// another planned crate the narrower plan no longer carries.
    /// A resolve without `unmoved` evidence attests nothing.
    fn independent_holds(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        unmoved: Option<&UnmovedResolve>,
        report: &ApplyReport,
    ) -> Vec<Change> {
        let Some(unmoved) = unmoved else {
            return Vec::new();
        };
        let applied: Vec<Change> = report
            .applied
            .iter()
            .filter(|change| plan.changes.contains(change))
            .cloned()
            .collect();
        let unapplied: BTreeSet<&str> = plan
            .changes
            .iter()
            .filter(|change| !applied.contains(change))
            .map(|change| change.package.name.as_str())
            .collect();
        let held: Vec<&Change> = {
            let constraints = self
                .refusal_constraints
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            report
                .skipped
                .iter()
                .map(|skipped| &skipped.change)
                .filter(|held| plan.changes.contains(held))
                .filter(|held| {
                    constraints
                        .get(&(project.root.clone(), rejection_key(held)))
                        .is_none_or(|names| {
                            names.iter().all(|name| !unapplied.contains(name.as_str()))
                        })
                })
                .collect()
        };
        held.into_iter()
            .filter(|held| {
                let mut narrowed = applied.clone();
                narrowed.push((*held).clone());
                self.landings
                    .recall(
                        &unmoved.key,
                        &unmoved.lock,
                        plan.rewrite,
                        &narrowed,
                        journal,
                        &project.generated_members,
                    )
                    .is_some_and(|warm| warm.files == unmoved.files)
            })
            .cloned()
            .collect()
    }

    /// Reads the resolved graph after the pin phase left `resolver_lock`.
    ///
    /// The read is unlocked, so it re-locks whatever the manifests no longer admit.
    /// Every pin-phase step was judged already, so a move this read makes is refused by failing the
    /// whole attempt with [`CoreError::UnacceptableResolve`], which the caller restores.
    async fn judged_graph(
        &self,
        scope: PinScope<'_>,
        resolver_lock: &CargoLock,
    ) -> Result<ResolvedGraph> {
        let PinScope {
            project,
            plan,
            journal,
            observer,
        } = scope;
        journal.validate_project(&project.root)?;
        let graph = self.metadata_with_observer(project, observer).await?;
        journal.validate_project(&project.root)?;
        let generated = &project.generated_members;
        let baseline = ResolveBaseline::capture(resolver_lock, generated);
        match baseline.violation(&read_lock(project)?, generated, plan) {
            Some(violation) => Err(CoreError::UnacceptableResolve(violation.to_string())),
            None => Ok(graph),
        }
    }

    async fn guard_after_resolve(
        &self,
        project: &Project,
        before: Option<&CargoLock>,
        after: &CargoLock,
        mut graph: Option<ResolvedGraph>,
        plan: &Plan,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<Option<ResolvedGraph>> {
        let initial = plan
            .initial_lock_snapshot
            .as_deref()
            .map(|bytes| {
                let text = std::str::from_utf8(bytes)
                    .map_err(|err| CoreError::LockUnreadable(err.to_string()))?;
                CargoLock::parse(text)
            })
            .transpose()?;
        let before = initial.as_ref().or(before);
        // Authored direct intent comes from Cargo's resolved edges, including renamed and
        // workspace-inherited declarations, after generated projections and edge policy settle.
        if graph.is_none()
            && !new_direct_lines(before, after, &project.generated_members).is_empty()
        {
            if let Some(observer) = observer {
                observer.resolver_started(None);
            }
            graph = Some(self.locked_reader().graph(project).await?);
        }
        guard_resolved_copies(
            before,
            after,
            graph.as_ref(),
            &plan.single_copy,
            &project.generated_members,
        )?;
        Ok(graph)
    }
}

/// Rewrites the generated `followers` a seed followed before its resolve so that they follow only
/// the moves that landed exactly in `after`.
///
/// Replaying only exact landings preserves removed or renamed projection entries.
fn replay_seeded_follows(
    root: &Utf8Path,
    snapshot: &[(Utf8PathBuf, Option<String>)],
    followers: &[MemberRef],
    moves: &[PlannedNodeMove],
    after: &CargoLock,
) -> Result<()> {
    for (relative, contents) in snapshot {
        if relative != Utf8Path::new("Cargo.lock")
            && let Some(contents) = contents
        {
            std::fs::write(root.join(relative), contents)?;
        }
    }
    for planned in moves {
        if after.has_crates_io_package(&planned.name, &planned.to)
            && !after.has_crates_io_package(&planned.name, &planned.from)
        {
            manifest::follow_constraint(
                root,
                followers,
                &planned.name,
                &planned.from,
                &planned.to,
            )?;
        }
    }
    Ok(())
}

fn add_collateral_changes(
    before: Option<&CargoLock>,
    after: &CargoLock,
    generated: &GeneratedMembers,
    report: &mut ApplyReport,
) {
    let declared = DeclaredSides::of(before, after, generated);
    let before = before
        .map(CargoLock::locked_versions_by_source)
        .unwrap_or_default();
    let after = after.locked_versions_by_source();
    // No paired version-slot change may be omitted.
    // Every moved slot the applied rows above do not already report is surfaced as a collateral
    // applied row: a transitive pushed backward for consistency, a crate matured down by `fix`,
    // or a *held* candidate the resolve still floated off its baseline (whose skip row alone
    // would hide that real move).
    let collateral = collateral_changes(&before, &after, &report.applied, &declared);
    tracing::debug!(
        before_slots = before.len(),
        after_slots = after.len(),
        collateral = collateral.len(),
        "cargo apply collateral diff"
    );
    report.applied.extend(collateral);
}

fn read_lock(project: &Project) -> Result<CargoLock> {
    let content = std::fs::read_to_string(project.root.join("Cargo.lock"))?;
    CargoLock::parse(&content)
}

/// The lock as the mutation journal captured it before anything moved, when the capture is
/// readable; the baseline every trial of the mutation starts from.
fn journal_preimage_lock(journal: &ProjectMutationJournal) -> Result<Option<CargoLock>> {
    journal
        .files()
        .iter()
        .find(|file| file.path() == Utf8Path::new("Cargo.lock"))
        .and_then(ProjectMutationFile::contents)
        .map(|bytes| {
            let text = std::str::from_utf8(bytes).map_err(|error| {
                CoreError::LockUnreadable(format!("journaled Cargo.lock: {error}"))
            })?;
            CargoLock::parse(text)
        })
        .transpose()
}

/// The `RewriteMode::Always` widen: every planned change's owning requirements, and the
/// projections of them, before any pin runs.
fn widen_all_up_front(
    project: &Project,
    plan: &Plan,
    journal: &ProjectMutationJournal,
    followers: &[MemberRef],
) -> Result<()> {
    for change in &plan.changes {
        journal.validate_project(&project.root)?;
        widen_and_follow(&project.root, change, followers, plan.rewrite)?;
    }
    Ok(())
}

/// The authored line a projection-retained node should follow: the nearest authored version
/// above it, else the highest — the line the retained node's requirers moved to.
fn companion_target<'a>(authored: &'a [String], retained: &str) -> Option<&'a str> {
    authored
        .iter()
        .filter(|version| version::compare(version, retained).is_gt())
        .min_by(|a, b| version::compare(a, b))
        .or_else(|| authored.iter().max_by(|a, b| version::compare(a, b)))
        .map(String::as_str)
}

/// Widens the declaring members' requirements on `change` to its target, then makes the
/// generated `followers`' projections of the moving version follow it — one rewrite step, so a
/// tentative widen and its follow are kept or restored together. Every widen is followed into
/// the generated manifests because the projection of the moving node must admit the target too,
/// or the resolver keeps the old version alive as a second copy for the projection's sake. The
/// union of the manifests either touched is what the caller judges progress by: a candidate only
/// the projection held back has nothing to widen and still needs its re-pin.
fn widen_and_follow(
    root: &Utf8Path,
    change: &Change,
    followers: &[MemberRef],
    mode: RewriteMode,
) -> Result<manifest::ManifestRewrite> {
    widen_and_follow_checked(root, change, followers, mode, || Ok(()))
}

fn widen_and_follow_checked(
    root: &Utf8Path,
    change: &Change,
    followers: &[MemberRef],
    mode: RewriteMode,
    mut validate: impl FnMut() -> Result<()>,
) -> Result<manifest::ManifestRewrite> {
    // A generated member attributed to the change (a node only its projection still holds) is
    // not an author to widen: the key-based, source-blind authored rewrite would touch whatever
    // entry of that name the projection carries, and with no author left there is no root
    // fallback to take either — the projection follows below, and that is all.
    let authors: Vec<MemberRef> = change
        .members
        .iter()
        .filter(|member| !followers.contains(member))
        .cloned()
        .collect();
    let mut rewrite = if authors.is_empty() && !change.members.is_empty() {
        manifest::ManifestRewrite::default()
    } else {
        manifest::widen_constraint_checked(
            root,
            &authors,
            &change.package.name,
            change.to.as_str(),
            mode,
            &mut validate,
        )?
    };
    if !followers.is_empty() {
        let followed = manifest::follow_constraint_checked(
            root,
            followers,
            &change.package.name,
            change.from.as_str(),
            change.to.as_str(),
            &mut validate,
        )?;
        rewrite.modified.extend(followed.modified);
    }
    Ok(rewrite)
}

/// An unlocking pin cargo carried out, with the state it started from.
struct UnlockedPin<'a> {
    change: &'a Change,
    baseline: &'a ResolveBaseline,
    /// The lock text before the pin, which a refused pin restores byte for byte.
    before_text: &'a str,
    released: &'a [PackageKey],
}

/// The `Cargo.lock` the journal captured, or `None` when it is missing or does not parse.
fn journaled_lock(journal: &ProjectMutationJournal) -> Option<CargoLock> {
    journal
        .files()
        .iter()
        .find(|file| file.path() == Utf8Path::new("Cargo.lock"))
        .and_then(ProjectMutationFile::contents)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|content| CargoLock::parse(content).ok())
}

/// The generated manifests whose journaled preimage differs from what is on disk after the
/// apply: every projection a landed move was followed into, which its generator now has to
/// rewrite.
fn followed_manifests(
    project: &Project,
    followers: &[MemberRef],
    journal: &ProjectMutationJournal,
) -> Result<Vec<Utf8PathBuf>> {
    let mut followed = Vec::new();
    for member in followers {
        let rel = manifest::member_manifest_rel(&member.path);
        let before = journal
            .files()
            .iter()
            .find(|file| file.path() == rel)
            .and_then(ProjectMutationFile::contents);
        let after = match std::fs::read(project.root.join(&rel)) {
            Ok(after) => after,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(CoreError::from(error)),
        };
        if before != Some(after.as_slice()) && !followed.contains(&rel) {
            followed.push(rel);
        }
    }
    Ok(followed)
}

/// Whether a planned candidate landed at its exact target in `after`.
///
/// Cargo receives a concrete `--precise` target, so an overshoot is not success: it may be inside
/// the manifest range but still younger than cooldown permits. Keyed per `(name, major)` slot; a
/// cross-major move is checked against the target's own major slot.
fn reached(after: &BTreeMap<SlotKey, String>, change: &Change) -> bool {
    let key = (
        change.package.name.clone(),
        version::major_key(change.to.as_str()).0,
    );
    after
        .get(&key)
        .is_some_and(|landed| landed == change.to.as_str())
}

fn needs_member_graph(change: &Change) -> bool {
    change.direct && !change.members.is_empty()
}

fn needs_apply_graph(changes: &[Change], after: &BTreeMap<SlotKey, String>) -> bool {
    changes
        .iter()
        .any(|change| needs_member_graph(change) || !reached(after, change))
}

fn resolved_copies(lock: &CargoLock) -> BTreeMap<(Option<String>, String), BTreeSet<String>> {
    let mut copies: BTreeMap<(Option<String>, String), BTreeSet<String>> = BTreeMap::new();
    for package in &lock.package {
        if let Some(version) = &package.version {
            copies
                .entry((package.source.clone(), package.name.clone()))
                .or_default()
                .insert(version.clone());
        }
    }
    copies
}

fn new_direct_lines(
    before: Option<&CargoLock>,
    after: &CargoLock,
    generated: &GeneratedMembers,
) -> BTreeSet<(Option<String>, String)> {
    let compatibility_lines = |lock: &CargoLock| {
        let mut lines: BTreeMap<(Option<String>, String), BTreeSet<String>> = BTreeMap::new();
        for (name, versions) in resolved_copies(lock) {
            lines
                .entry(name)
                .or_default()
                .extend(versions.iter().map(|version| compatibility_line(version)));
        }
        lines
    };
    let before_lines = before.map(compatibility_lines).unwrap_or_default();
    compatibility_lines(after)
        .into_iter()
        .filter(|(name, lines)| {
            lines.len() > 1
                && lines.iter().any(|line| {
                    !before_lines
                        .get(name)
                        .is_some_and(|before| before.contains(line))
                })
                && before.is_none_or(|before| splits_requirers(before, after, name, generated))
        })
        .map(|(name, _)| name)
        .collect()
}

/// Whether `after` gives the crate `identity` a split that `before` did not already have.
///
/// A requirer is one package of the lock, and binds the compatibility lines of the crate's blocks
/// its dependency entries resolve to.
/// A requirer of `after` continues the same package of `before`, or the one version of its name
/// and source that it replaced when each lock binds the crate from exactly one version of it.
/// The resolve splits the crate when an authored member and another requirer that shared a line
/// no longer share any, or when a requirer that continues none binds a line `before` did not
/// carry while a member declares the crate.
/// A line no requirer binds gives no evidence either way and counts as a split.
/// Generated members only mirror the authored ones, so they neither split nor retain.
/// A member moving off a line no other requirer kept leaves a standing split as it was, so a
/// direct dependency already apart from its dependents' lines may move to a newer one.
fn splits_requirers(
    before: &CargoLock,
    after: &CargoLock,
    identity: &(Option<String>, String),
    generated: &GeneratedMembers,
) -> bool {
    let before_bound = requirer_lines(before, identity, generated);
    let after_bound = requirer_lines(after, identity, generated);
    let carried = |lock: &CargoLock| -> BTreeSet<String> {
        resolved_copies(lock)
            .remove(identity)
            .unwrap_or_default()
            .iter()
            .map(|version| compatibility_line(version))
            .collect()
    };
    let new_lines: BTreeSet<String> = carried(after)
        .difference(&carried(before))
        .cloned()
        .collect();
    let after_lines: BTreeSet<&String> = after_bound.values().flatten().collect();
    if new_lines.iter().any(|line| !after_lines.contains(line)) {
        return true;
    }
    let versions_of = |bound: &BTreeMap<Requirer, BTreeSet<String>>, requirer: &Requirer| {
        bound
            .keys()
            .filter(|other| other.name == requirer.name && other.source == requirer.source)
            .count()
    };
    let predecessor = |requirer: &Requirer| -> Option<&BTreeSet<String>> {
        before_bound.get(requirer).or_else(|| {
            let mut replaced = before_bound.iter().filter(|(other, _)| {
                other.name == requirer.name && other.source == requirer.source
            });
            match (replaced.next(), replaced.next()) {
                (Some((_, lines)), None) if versions_of(&after_bound, requirer) == 1 => Some(lines),
                _ => None,
            }
        })
    };
    if after_bound.keys().any(Requirer::is_authored)
        && after_bound.iter().any(|(requirer, lines)| {
            predecessor(requirer).is_none() && !lines.is_disjoint(&new_lines)
        })
    {
        return true;
    }
    after_bound
        .iter()
        .filter(|(member, _)| member.is_authored())
        .filter_map(|(member, after)| Some((member, predecessor(member)?, after)))
        .any(|(member, member_before, member_after)| {
            after_bound.iter().any(|(requirer, after)| {
                requirer != member
                    && predecessor(requirer).is_some_and(|before| {
                        !member_before.is_disjoint(before) && member_after.is_disjoint(after)
                    })
            })
        })
}

/// One package of a lock that binds a crate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Requirer {
    name: String,
    version: Option<String>,
    source: Option<String>,
}

impl Requirer {
    /// A path package, which is a workspace member or a path dependency of one.
    fn is_authored(&self) -> bool {
        self.source.is_none()
    }
}

/// The compatibility lines of `identity` each requirer other than a generated member binds.
fn requirer_lines(
    lock: &CargoLock,
    identity: &(Option<String>, String),
    generated: &GeneratedMembers,
) -> BTreeMap<Requirer, BTreeSet<String>> {
    let (source, name) = identity;
    let mut bound: BTreeMap<Requirer, BTreeSet<String>> = BTreeMap::new();
    for edge in lock.edges() {
        if edge.endpoint.name != *name
            || edge.endpoint.source != *source
            || (edge.requirer.source.is_none() && generated.contains(&edge.requirer.name))
        {
            continue;
        }
        let Some(version) = &edge.endpoint.version else {
            continue;
        };
        bound
            .entry(Requirer {
                name: edge.requirer.name.clone(),
                version: edge.requirer.version.clone(),
                source: edge.requirer.source.clone(),
            })
            .or_default()
            .insert(compatibility_line(version));
    }
    bound
}

fn compatibility_line(version: &str) -> String {
    // Cargo treats different 0.0.x patches as incompatible, too.
    match version::parse(version) {
        Some(parsed) if parsed.major == 0 && parsed.minor == 0 => format!("0.0.{}", parsed.patch),
        _ => version::major_key(version).0,
    }
}

/// Refuses new incompatible direct lines and policy-gated growth in distinct resolved versions.
fn guard_resolved_copies(
    before: Option<&CargoLock>,
    after: &CargoLock,
    graph: Option<&ResolvedGraph>,
    single_copy: &cooldown_core::SingleCopyPolicy,
    generated: &GeneratedMembers,
) -> Result<()> {
    let after_copies = resolved_copies(after);
    let before_copies = before.map(resolved_copies).unwrap_or_default();
    for (identity, versions) in &after_copies {
        let (_, name) = identity;
        if let Some(gate) = single_copy.gate_of(name)
            && versions.len()
                > before_copies
                    .get(identity)
                    .map_or(1, |before| before.len().max(1))
        {
            return Err(CoreError::UnacceptableResolve(format!(
                "would resolve {name} at another version ({}), which {gate} forbids; upgrade its dependents together so the copies converge",
                versions.iter().cloned().collect::<Vec<_>>().join(" and ")
            )));
        }
    }
    let Some(graph) = graph else {
        return Ok(());
    };
    for identity in new_direct_lines(before, after, generated) {
        let (source, name) = &identity;
        if graph
            .packages
            .iter()
            .any(|(id, info)| info.name == *name && info.source == *source && graph.is_direct(id))
        {
            let versions = versions_for_name(&after_copies, &identity).join(" and ");
            let remedy = direct_split_remedy(graph, &identity, before_copies.get(&identity));
            return Err(CoreError::UnacceptableResolve(format!(
                "would resolve direct dependency {name} at another incompatible line ({versions}); {remedy}"
            )));
        }
    }
    Ok(())
}

fn direct_split_remedy(
    graph: &ResolvedGraph,
    identity: &(Option<String>, String),
    before: Option<&BTreeSet<String>>,
) -> String {
    let (source, name) = identity;
    let retained: HashSet<&String> = graph
        .packages
        .iter()
        .filter(|(_, info)| {
            info.name == *name
                && info.source == *source
                && before.is_some_and(|versions| {
                    versions.iter().any(|version| {
                        compatibility_line(version) == compatibility_line(&info.version)
                    })
                })
        })
        .map(|(id, _)| id)
        .collect();
    let requirers: BTreeSet<String> = graph
        .edges
        .iter()
        .filter(|(id, dependencies)| {
            !graph.generated_roots.contains(*id)
                && dependencies
                    .iter()
                    .any(|dependency| retained.contains(dependency))
        })
        .filter_map(|(id, _)| graph.packages.get(id))
        .map(|info| format!("{} {}", info.name, info.version))
        .collect();
    let attribution = if requirers.is_empty() {
        "a dependency retains the old line".to_string()
    } else {
        format!(
            "the old line is retained by {}",
            requirers.into_iter().collect::<Vec<_>>().join(", ")
        )
    };
    format!(
        "{attribution}; align the retaining dependents' requirements and compatible releases, or keep this candidate held"
    )
}

fn versions_for_name(
    copies: &BTreeMap<(Option<String>, String), BTreeSet<String>>,
    identity: &(Option<String>, String),
) -> Vec<String> {
    copies
        .get(identity)
        .into_iter()
        .flat_map(|versions| versions.iter().cloned())
        .collect()
}

fn reached_after(
    after: &BTreeMap<SlotKey, String>,
    graph: Option<&ResolvedGraph>,
    change: &Change,
) -> bool {
    if needs_member_graph(change) {
        return graph.is_some_and(|graph| {
            graph.direct_members_reach(
                &change.members,
                &change.package.name,
                change.from.as_str(),
                change.to.as_str(),
            )
        });
    }
    reached(after, change)
}

#[async_trait]
impl ToolWrite for CargoTool {
    fn mutation_tool(&self) -> ToolId {
        CARGO_ID
    }

    fn sync_scope(&self) -> SyncScope {
        SyncScope::Repo
    }

    async fn write_repo_native(
        &self,
        repo_root: &Utf8Path,
        policy: &ResolvedPolicy,
        dry_run: bool,
    ) -> Result<SyncReport> {
        crate::publish_age::write_repo_native(
            repo_root,
            policy,
            &CargoEnvironment::current(),
            dry_run,
        )
    }

    async fn repo_native_policy(&self, repo_root: &Utf8Path) -> Result<Option<NativePolicyLayer>> {
        native_policy_layer(
            repo_root,
            &repo_root.join(crate::CARGO_MANIFEST),
            &CargoEnvironment::current(),
        )
    }

    fn supports_transitive_advance(&self) -> bool {
        // The per-spec `update -p name@from --precise to` pin addresses any locked crate, declared
        // or not.
        true
    }

    fn guards_duplicate_copies(&self) -> bool {
        true
    }

    fn target_fallback(&self) -> TargetFallback {
        TargetFallback::LowerOnSelectedLine
    }

    fn resolve_inputs(&self) -> ResolveInputs {
        // Only the in-place path reads this — the isolated strategy stages its own authoritative
        // read set — so this is what a non-Unix preview copy is assembled from.
        // `cargo update`/`generate-lockfile` validates every workspace member's declared targets,
        // so the throwaway copy must include `.rs` source: a member with an empty `src/` errors
        // with "no targets specified".
        // Source is small (the bulk of a repo is build `target/`, which is pruned).
        ResolveInputs {
            source_extensions: &["rs"],
            ..ResolveInputs::DEFAULT
        }
    }

    fn mutation_execution(&self) -> MutationExecution<'_> {
        // Publishing an isolated trial needs recovery authority, which coordination only grants
        // where the platform can prove its namespace is private to the current user.
        // Committing to the isolated path on a platform that never grants it would fail every
        // Cargo mutation closed at publication rather than degrade, so those platforms fall
        // back to the in-place trial-and-rollback path every other ecosystem already uses.
        // A non-Git project on a supported platform still fails closed: what it lacks is an
        // authority anchor outside project content, not platform support.
        if RecoveryAuthority::platform_supported() {
            MutationExecution::Isolated(self)
        } else {
            MutationExecution::InPlace
        }
    }

    async fn ensure_no_pending_mutation(&self, project: &Project) -> Result<()> {
        crate::staging::reject_custom_lockfile(&project.root)?;
        edges::enforce::ensure_no_pending(project)
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        // Capture the lock and every manifest a rewrite could touch (the root, for
        // `[workspace.dependencies]`, plus each declaring member) so a rejected trial rolls back
        // both the re-lock and any constraint edit. Capturing an unmodified manifest is harmless —
        // restore only runs on rollback and rewrites identical bytes.
        let mut relative: BTreeSet<Utf8PathBuf> = BTreeSet::new();
        relative.insert(Utf8PathBuf::from("Cargo.lock"));
        relative.insert(Utf8PathBuf::from("Cargo.toml"));
        for change in &plan.changes {
            for member in &change.members {
                relative.insert(manifest::member_manifest_rel(&member.path));
            }
        }
        // A generated member's manifest is rewritten whenever a move is followed into it, so it
        // is journaled as an output like any declaring member's — and the preimage is what the
        // apply report later compares against to say which projections followed.
        for member in &self.generated_facts(project, None).await?.members {
            relative.insert(manifest::member_manifest_rel(&member.path));
        }
        ProjectMutationJournal::capture(&project.root, relative)
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, journal) = self.mutation_parts(mutation)?;
        self.apply_plan(project, plan, journal, None).await
    }

    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        let (project, plan, journal) = self.mutation_parts(mutation)?;
        let report = self
            .apply_plan(project, plan, journal, Some(observer))
            .await;
        let report = match report {
            Err(CoreError::PendingRecovery(detail)) => {
                return Ok(mutation.pending_recovery_attempt(detail));
            }
            report => report,
        };
        let postimage = journal.capture_state()?;
        mutation.finished_attempt(report, &postimage)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cargo.build(&project.root).await
    }

    async fn refresh_lock(&self, project: &Project) -> Result<Option<LockVerifyReport>> {
        // The same preconditions as every other read of the lock: a custom lockfile name is not
        // ours to rewrite, and pending edge-normalization state must be settled first.
        crate::staging::reject_custom_lockfile(&project.root)?;
        edges::enforce::ensure_no_pending(project)?;
        self.cargo.refresh_lock(&project.root).await.map(Some)
    }

    fn supports_lock_refresh(&self) -> bool {
        true
    }

    /// Releases the locked nodes Cargo's version-selection conflict names, round by round, until
    /// the lock resolves: `cargo update --workspace` keeps every node the manifests still admit
    /// on its own, so a raised floor that a locked dependent's requirement also binds (handlebars
    /// holding pest 2.9.2 against a path crate's new `pest = "2.9.3"`) never resolves through it.
    async fn refresh_conflicted_lock(&self, project: &Project) -> Result<Option<LockVerifyReport>> {
        crate::staging::reject_custom_lockfile(&project.root)?;
        edges::enforce::ensure_no_pending(project)?;
        let Err(mut error) = self
            .cargo
            .verify_locked(&project.root, &project.generated_members)
            .await
        else {
            return Ok(None);
        };
        let mut released: Vec<PackageKey> = Vec::new();
        for _ in 0..LOCK_CONFLICT_ROUNDS {
            let mut blockers = locked_parties(&read_lock(project)?, &rejection_parties(&error));
            blockers.retain(|node| !released.contains(node));
            if blockers.is_empty() {
                break;
            }
            released.append(&mut blockers);
            match self.cargo.update_unlocking(&project.root, &released).await {
                Ok(()) => {
                    let nodes: Vec<String> = released
                        .iter()
                        .map(|node| format!("{}@{}", node.name, node.version))
                        .collect();
                    return Ok(Some(LockVerifyReport {
                        status: LockStatus::Current,
                        detail: format!(
                            "Cargo.lock re-resolved with {} unlocked",
                            nodes.join(", ")
                        ),
                    }));
                }
                Err(err) if err.is_local_environment_failure() || err.is_tool_spawn_failure() => {
                    return Err(err);
                }
                Err(err) => error = err,
            }
        }
        Err(error)
    }

    async fn recover_pending_mutation(
        &self,
        project: &Project,
        coordination: &cooldown_core::fs::ProjectCoordination,
    ) -> Result<cooldown_core::MutationRecovery> {
        // Every mutation flow calls this before touching the project, so a project that never
        // published a record must settle as unchanged rather than fail closed. Without recovery
        // authority nothing could have been published, and demanding it here would reject every
        // Cargo mutation on the in-place path such a platform selects.
        // Artifacts that are present still require authority: untrusted project bytes cannot
        // authorize restoration.
        if coordination.recovery_authority().is_none()
            && !crate::publication::has_project_recovery_artifacts(
                &project.root.join("Cargo.lock"),
            )?
        {
            return Ok(cooldown_core::MutationRecovery::settled(
                cooldown_core::RecoveryDisposition::Unchanged,
            ));
        }
        let authority = crate::publication::require_recovery_authority(project, coordination)?;
        edges::enforce::recover_pending(project, authority)
    }

    async fn lock_edge_snapshot(&self, project: &Project) -> Result<Option<Vec<u8>>> {
        std::fs::read(project.root.join("Cargo.lock"))
            .map(Some)
            .map_err(Into::into)
    }

    /// Final edge-binding enforcement and run-start-to-final audit.
    async fn normalize_lock_edges(
        &self,
        mutation: &PreparedMutation,
        policy: EdgePolicy,
        before: Option<&[u8]>,
        committed: &[EdgeRebind],
    ) -> Result<EdgeNormalizationReport> {
        let (project, _, _) = self.mutation_parts(mutation)?;
        let before_lock = before
            .map(|contents| {
                std::str::from_utf8(contents)
                    .map_err(|error| {
                        CoreError::LockUnreadable(format!("Cargo.lock snapshot: {error}"))
                    })
                    .and_then(CargoLock::parse)
            })
            .transpose()?;
        let current_lock = read_lock(project)?;
        let graph = match policy {
            EdgePolicy::None => None,
            EdgePolicy::Preserve => {
                let needs_graph = before_lock.as_ref().is_some_and(|before_lock| {
                    edges::preserve::has_potential_restoration(
                        &edges::LockEdgeView::from_lock(before_lock),
                        &edges::LockEdgeView::from_lock(&current_lock),
                    )
                });
                if needs_graph {
                    Some(
                        self.cargo
                            .metadata(&project.root, &project.generated_members)
                            .await?,
                    )
                } else {
                    None
                }
            }
            // A metadata failure must remain a project error; otherwise requested canonical
            // healing would degrade to a successful no-op.
            EdgePolicy::Canonicalize => Some(
                self.cargo
                    .metadata(&project.root, &project.generated_members)
                    .await?,
            ),
        };
        let generated = self.generated_facts(project, None).await?;
        let mut result = edges::enforce::enforce(
            self.locked_reader(),
            project,
            policy,
            before_lock.as_ref(),
            graph,
            &generated.identities,
            None,
        )
        .await?;
        let final_view = edges::LockEdgeView::from_lock(&read_lock(project)?);
        edges::enforce::reconcile_committed_outcomes(&final_view, &mut result.rebinds, committed);
        Ok(EdgeNormalizationReport {
            rebinds: result.rebinds,
            warnings: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod seeded_pins;

    #[cfg(unix)]
    mod widen_probe;

    #[cfg(unix)]
    mod rejection_memo;

    #[cfg(unix)]
    mod unlocking_pins;

    #[cfg(unix)]
    mod acceptance_steps;

    #[cfg(unix)]
    mod split_completion;

    fn duplicate_guard_lock(versions: &[(&str, &str)]) -> eyre::Result<CargoLock> {
        let mut content = String::new();
        for (name, version) in versions {
            content.push_str(&indoc::formatdoc! {r#"
                [[package]]
                name = "{name}"
                version = "{version}"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#});
        }
        Ok(CargoLock::parse(&content)?)
    }

    #[test]
    fn duplicate_guard_rejects_authored_direct_lines_but_not_generated_only() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let mut graph = hakari_workspace(root);
        graph.generated_roots.insert("hack".to_string());
        let before = duplicate_guard_lock(&[("dep", "1.0.0"), ("orphan", "2.0.0")])?;
        let after = duplicate_guard_lock(&[
            ("dep", "1.0.0"),
            ("dep", "2.0.0"),
            ("orphan", "2.0.0"),
            ("orphan", "3.0.0"),
        ])?;
        let error = guard_resolved_copies(
            Some(&before),
            &after,
            Some(&graph),
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        )
        .expect_err("authored dep cannot gain an incompatible line");
        assert!(matches!(error, CoreError::UnacceptableResolve(ref detail)
            if detail.contains("direct dependency dep") && detail.contains("1.0.0 and 2.0.0")
            && detail.contains("retained by app 0.1.0")
            && detail.contains("keep this candidate held")));

        // A pre-existing split does not authorize replacing one line with a new incompatible one.
        let standing = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "3.0.0")])?;
        assert!(matches!(
            guard_resolved_copies(
                Some(&standing),
                &after,
                Some(&graph),
                &cooldown_core::SingleCopyPolicy::default(),
                &GeneratedMembers::undeclared()
            ),
            Err(CoreError::UnacceptableResolve(_))
        ));

        // Removing the authored declaration leaves only generated projection intent.
        graph
            .edges
            .get_mut("app")
            .expect("app edges")
            .retain(|id| id != "dep");
        guard_resolved_copies(
            Some(&before),
            &after,
            Some(&graph),
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        )?;
        Ok(())
    }

    #[test]
    fn duplicate_guard_names_retainers_when_the_direct_dependency_moves() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let mut graph = hakari_workspace(root);
        graph.generated_roots.insert("hack".to_string());
        // The authored edge moves to the new line, while two registry packages keep the old one.
        graph.packages.insert(
            "new-dep".to_string(),
            crate::cargocmd::PkgInfo {
                name: "dep".to_string(),
                version: "2.0.0".to_string(),
                source: Some(crate::cargocmd::CRATES_IO_SOURCE.to_string()),
                path: String::new(),
            },
        );
        graph
            .edges
            .insert("app".to_string(), vec!["new-dep".to_string()]);
        for name in ["retainer-z", "retainer-a"] {
            graph.packages.insert(
                name.to_string(),
                crate::cargocmd::PkgInfo {
                    name: name.to_string(),
                    version: "3.0.0".to_string(),
                    source: Some(crate::cargocmd::CRATES_IO_SOURCE.to_string()),
                    path: String::new(),
                },
            );
            graph
                .edges
                .insert(name.to_string(), vec!["dep".to_string()]);
        }
        // A source-distinct namesake must not be blamed for retaining the registry copy.
        graph.packages.insert(
            "private-dep".to_string(),
            crate::cargocmd::PkgInfo {
                name: "dep".to_string(),
                version: "1.0.0".to_string(),
                source: Some("registry+https://private.example/index".to_string()),
                path: String::new(),
            },
        );
        graph
            .edges
            .insert("orphan".to_string(), vec!["private-dep".to_string()]);
        let before = duplicate_guard_lock(&[("dep", "1.0.0")])?;
        let after = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "2.0.0")])?;
        let error = guard_resolved_copies(
            Some(&before),
            &after,
            Some(&graph),
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        )
        .expect_err("the direct upgrade must not add an incompatible copy");
        let CoreError::UnacceptableResolve(detail) = error else {
            eyre::bail!("unexpected rejection: {error}");
        };
        assert!(
            detail.contains("retained by retainer-a 3.0.0, retainer-z 3.0.0"),
            "{detail}"
        );
        assert!(
            detail.contains("align the retaining dependents' requirements and compatible releases")
        );
        assert!(detail.contains("keep this candidate held"));
        assert!(!detail.contains("upgrade dep with it"));
        assert!(!detail.contains("hack 0.1.0"));
        assert!(!detail.contains("orphan 2.0.0"));
        Ok(())
    }

    #[test]
    fn duplicate_guard_allows_standing_split_and_ordinary_major_move() -> eyre::Result<()> {
        let before = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "2.0.0")])?;
        let after = duplicate_guard_lock(&[("dep", "1.1.0"), ("dep", "2.1.0")])?;
        let policy = cooldown_core::SingleCopyPolicy {
            every_name: true,
            ..Default::default()
        };
        assert!(
            new_direct_lines(Some(&before), &after, &GeneratedMembers::undeclared()).is_empty()
        );
        guard_resolved_copies(
            Some(&before),
            &after,
            None,
            &policy,
            &GeneratedMembers::undeclared(),
        )?;
        let before = duplicate_guard_lock(&[("dep", "1.0.0")])?;
        let after = duplicate_guard_lock(&[("dep", "2.0.0")])?;
        assert!(
            new_direct_lines(Some(&before), &after, &GeneratedMembers::undeclared()).is_empty()
        );
        guard_resolved_copies(
            Some(&before),
            &after,
            None,
            &policy,
            &GeneratedMembers::undeclared(),
        )?;
        Ok(())
    }

    /// A lock of `app` and registry crates, each `(name, version, dependency entries)`.
    fn requirer_lock(packages: &[(&str, &str, &[&str])]) -> eyre::Result<CargoLock> {
        let mut content = String::new();
        for (name, version, dependencies) in packages {
            let source = if *name == "app" {
                String::new()
            } else {
                "source = \"registry+https://github.com/rust-lang/crates.io-index\"\n".to_string()
            };
            let dependencies = serde_json::to_string(dependencies)?;
            content.push_str(&formatdoc! {r#"
                [[package]]
                name = "{name}"
                version = "{version}"
                {source}dependencies = {dependencies}
            "#});
        }
        Ok(CargoLock::parse(&content)?)
    }

    /// A direct dependency already on its own line beside its dependents' older lines may move to
    /// a newer line: no requirer that shared a line with it is left behind.
    #[test]
    fn duplicate_guard_lets_a_direct_dependency_leave_a_standing_split() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let graph = hakari_workspace(root);
        let before = requirer_lock(&[
            ("app", "0.1.0", &["dep 0.45.0", "extract", "print"]),
            ("extract", "0.12.1", &["dep 0.42.0"]),
            ("print", "0.12.8", &["dep 0.44.0"]),
            ("dep", "0.42.0", &[]),
            ("dep", "0.44.0", &[]),
            ("dep", "0.45.0", &[]),
        ])?;
        let after = requirer_lock(&[
            ("app", "0.1.0", &["dep 0.46.0", "extract", "print"]),
            ("extract", "0.12.1", &["dep 0.42.0"]),
            ("print", "0.12.8", &["dep 0.44.0"]),
            ("dep", "0.42.0", &[]),
            ("dep", "0.44.0", &[]),
            ("dep", "0.46.0", &[]),
        ])?;
        assert!(
            new_direct_lines(Some(&before), &after, &GeneratedMembers::undeclared()).is_empty()
        );
        guard_resolved_copies(
            Some(&before),
            &after,
            Some(&graph),
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        )?;
        Ok(())
    }

    /// Moving a direct dependency off a line another requirer keeps is a new split even when the
    /// number of lines stays the same.
    #[test]
    fn duplicate_guard_refuses_leaving_a_shared_line_behind() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let graph = hakari_workspace(root);
        let before = requirer_lock(&[
            ("app", "0.1.0", &["dep 2.0.0", "lower", "sharer"]),
            ("lower", "1.0.0", &["dep 1.0.0"]),
            ("sharer", "1.0.0", &["dep 2.0.0"]),
            ("dep", "1.0.0", &[]),
            ("dep", "2.0.0", &[]),
        ])?;
        // `lower` moves onto the new line with `app`, so 1.x disappears while `sharer` keeps 2.x.
        let after = requirer_lock(&[
            ("app", "0.1.0", &["dep 3.0.0", "lower", "sharer"]),
            ("lower", "2.0.0", &["dep 3.0.0"]),
            ("sharer", "1.0.0", &["dep 2.0.0"]),
            ("dep", "2.0.0", &[]),
            ("dep", "3.0.0", &[]),
        ])?;
        let result = guard_resolved_copies(
            Some(&before),
            &after,
            Some(&graph),
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        );
        assert!(
            matches!(&result, Err(CoreError::UnacceptableResolve(detail)) if detail.contains("another incompatible line")),
            "{result:?}"
        );
        // A requirer that did not depend on the crate before and lands on the new line splits it
        // as well.
        let before = requirer_lock(&[
            ("app", "0.1.0", &["dep 2.0.0", "lower"]),
            ("lower", "1.0.0", &["dep 1.0.0"]),
            ("dep", "1.0.0", &[]),
            ("dep", "2.0.0", &[]),
        ])?;
        let after = requirer_lock(&[
            ("app", "0.1.0", &["dep 2.0.0", "lower", "newcomer"]),
            ("lower", "1.0.0", &["dep 1.0.0"]),
            ("newcomer", "1.0.0", &["dep 3.0.0"]),
            ("dep", "1.0.0", &[]),
            ("dep", "2.0.0", &[]),
            ("dep", "3.0.0", &[]),
        ])?;
        assert!(
            !new_direct_lines(Some(&before), &after, &GeneratedMembers::undeclared()).is_empty()
        );
        Ok(())
    }

    /// Each version of a requirer keeps its own lines: an unchanged version left on the old
    /// line splits from the member even when another version of its name moved along.
    #[test]
    fn duplicate_guard_tells_coexisting_requirer_versions_apart() -> eyre::Result<()> {
        let before = requirer_lock(&[
            ("app", "0.1.0", &["dep 1.0.0", "wrapper 1.0.0"]),
            ("wrapper", "1.0.0", &["dep 1.0.0"]),
            ("other", "1.0.0", &["wrapper 2.0.0"]),
            ("wrapper", "2.0.0", &["dep 2.0.0"]),
            ("dep", "1.0.0", &[]),
            ("dep", "2.0.0", &[]),
        ])?;
        let after = requirer_lock(&[
            ("app", "0.1.0", &["dep 3.0.0", "wrapper 1.0.0"]),
            ("wrapper", "1.0.0", &["dep 1.0.0"]),
            ("other", "1.0.0", &["wrapper 3.0.0"]),
            ("wrapper", "3.0.0", &["dep 3.0.0"]),
            ("dep", "1.0.0", &[]),
            ("dep", "3.0.0", &[]),
        ])?;
        assert!(
            !new_direct_lines(Some(&before), &after, &GeneratedMembers::undeclared()).is_empty()
        );
        Ok(())
    }

    /// A dependency moving onto the new line together with the member is the same requirer in
    /// its new version, so the pair stays together and no split is reported.
    #[test]
    fn duplicate_guard_lets_a_requirer_move_along_with_the_member() -> eyre::Result<()> {
        let before = requirer_lock(&[
            ("app", "0.1.0", &["dep 1.0.0", "follower", "holder"]),
            ("follower", "0.3.0", &["dep 1.0.0"]),
            ("holder", "1.0.0", &["dep 0.9.0"]),
            ("dep", "0.9.0", &[]),
            ("dep", "1.0.0", &[]),
        ])?;
        let after = requirer_lock(&[
            ("app", "0.1.0", &["dep 2.0.0", "follower", "holder"]),
            ("follower", "0.4.0", &["dep 2.0.0"]),
            ("holder", "1.0.0", &["dep 0.9.0"]),
            ("dep", "0.9.0", &[]),
            ("dep", "2.0.0", &[]),
        ])?;
        assert!(
            new_direct_lines(Some(&before), &after, &GeneratedMembers::undeclared()).is_empty()
        );
        Ok(())
    }

    #[test]
    fn duplicate_guard_uses_cargo_zero_major_compatibility() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let graph = hakari_workspace(root);
        for (baseline, target, incompatible) in [
            ("0.136.1", "0.136.2", false),
            ("0.136.1", "0.137.1", true),
            ("0.0.1", "0.0.2", true),
        ] {
            let before = duplicate_guard_lock(&[("dep", baseline)])?;
            let after = duplicate_guard_lock(&[("dep", baseline), ("dep", target)])?;
            let result = guard_resolved_copies(
                Some(&before),
                &after,
                Some(&graph),
                &cooldown_core::SingleCopyPolicy::default(),
                &GeneratedMembers::undeclared(),
            );
            assert_eq!(result.is_err(), incompatible, "{baseline} and {target}");
        }
        Ok(())
    }

    #[test]
    fn duplicate_guard_flag_rejects_transitive_versions_in_same_compatible_line() -> eyre::Result<()>
    {
        let before = duplicate_guard_lock(&[("dep", "1.0.0")])?;
        let after = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "1.1.0")])?;
        assert!(
            new_direct_lines(Some(&before), &after, &GeneratedMembers::undeclared()).is_empty()
        );
        guard_resolved_copies(
            Some(&before),
            &after,
            None,
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        )?;
        let policy = cooldown_core::SingleCopyPolicy {
            every_name: true,
            ..Default::default()
        };
        assert!(
            matches!(guard_resolved_copies(Some(&before), &after, None, &policy, &GeneratedMembers::undeclared()),
            Err(CoreError::UnacceptableResolve(detail)) if detail.contains("--fail-on-new-duplicate"))
        );
        Ok(())
    }

    #[test]
    fn duplicate_guard_covers_authored_dependencies_outside_crates_io() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        // The direct-dependency invariant also covers private registries, Git and path sources.
        for source in [
            Some("registry+https://private.example/index"),
            Some("git+https://example.com/repo#abc"),
            None,
        ] {
            let mut graph = hakari_workspace(root);
            graph
                .packages
                .get_mut("dep")
                .ok_or_else(|| eyre::eyre!("missing direct crate"))?
                .source = source.map(str::to_string);
            let mut before = duplicate_guard_lock(&[("dep", "1.0.0")])?;
            let mut after = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "2.0.0")])?;
            for package in before.package.iter_mut().chain(&mut after.package) {
                package.source = source.map(str::to_string);
            }
            assert!(matches!(
                guard_resolved_copies(
                    Some(&before),
                    &after,
                    Some(&graph),
                    &cooldown_core::SingleCopyPolicy::default(),
                    &GeneratedMembers::undeclared()
                ),
                Err(CoreError::UnacceptableResolve(_))
            ));
        }
        Ok(())
    }

    #[test]
    fn seed_rejects_a_target_slot_drive_by_while_the_original_line_survives() -> eyre::Result<()> {
        let before = duplicate_guard_lock(&[("dep", "1.0.0")])?;
        let after = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "2.2.0")])?;
        assert!(seed_has_drive_by(
            &before,
            &after,
            &[PlannedNodeMove {
                name: "dep".to_string(),
                from: "1.0.0".to_string(),
                to: "2.1.0".to_string()
            }]
        ));
        Ok(())
    }

    #[test]
    fn duplicate_guard_keeps_source_baselines_and_direct_intent_separate() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let graph = hakari_workspace(root);
        let mut before = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "2.0.0")])?;
        before
            .package
            .get_mut(1)
            .ok_or_else(|| eyre::eyre!("private package"))?
            .source = Some("registry+https://private.example/index".to_string());
        let mut after = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "2.0.0")])?;
        after
            .package
            .get_mut(1)
            .ok_or_else(|| eyre::eyre!("private package"))?
            .source = Some("registry+https://private.example/index".to_string());
        after
            .package
            .extend(duplicate_guard_lock(&[("dep", "2.0.0")])?.package);
        assert!(matches!(
            guard_resolved_copies(
                Some(&before),
                &after,
                Some(&graph),
                &cooldown_core::SingleCopyPolicy::default(),
                &GeneratedMembers::undeclared()
            ),
            Err(CoreError::UnacceptableResolve(_))
        ));
        let mut private_move = duplicate_guard_lock(&[("dep", "1.0.0"), ("dep", "2.0.0")])?;
        private_move
            .package
            .get_mut(1)
            .ok_or_else(|| eyre::eyre!("private package"))?
            .source = Some("registry+https://private.example/index".to_string());
        private_move
            .package
            .get_mut(1)
            .ok_or_else(|| eyre::eyre!("private package"))?
            .version = Some("3.0.0".to_string());
        guard_resolved_copies(
            Some(&before),
            &private_move,
            Some(&graph),
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        )?;
        // A newly split private line has no authored direct intent in the crates.io graph.
        for lock in [&mut before, &mut private_move] {
            let mut extra = duplicate_guard_lock(&[("dep", "4.0.0")])?;
            for package in &mut extra.package {
                package.source = Some("registry+https://private.example/index".to_string());
            }
            lock.package.extend(extra.package);
        }
        assert!(
            !new_direct_lines(
                Some(&before),
                &private_move,
                &GeneratedMembers::undeclared()
            )
            .is_empty()
        );
        guard_resolved_copies(
            Some(&before),
            &private_move,
            Some(&graph),
            &cooldown_core::SingleCopyPolicy::default(),
            &GeneratedMembers::undeclared(),
        )?;
        Ok(())
    }

    /// Remediation may restore a pre-existing line that an earlier batch temporarily removed.
    #[cfg(unix)]
    #[tokio::test]
    async fn duplicate_guard_uses_the_run_snapshot_across_batches() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let (project, tool) = atomic_fixture(root, "")?;
        let initial = indoc! {r#"
            version = 3
            [[package]]
            name = "dep"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            [[package]]
            name = "dep"
            version = "2.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#};
        let before = duplicate_guard_lock(&[("dep", "2.0.0")])?;
        let after = CargoLock::parse(initial)?;
        let graph = hakari_workspace(root);
        let policy = cooldown_core::SingleCopyPolicy {
            every_name: true,
            ..cooldown_core::SingleCopyPolicy::default()
        };
        assert!(matches!(
            guard_resolved_copies(
                Some(&before),
                &after,
                Some(&graph),
                &policy,
                &GeneratedMembers::undeclared()
            ),
            Err(CoreError::UnacceptableResolve(_))
        ));
        let plan = Plan {
            single_copy: policy,
            initial_lock_snapshot: Some(Arc::from(initial.as_bytes())),
            ..Plan::default()
        };
        tool.guard_after_resolve(&project, Some(&before), &after, Some(graph), &plan, None)
            .await?;
        Ok(())
    }

    /// The inherited alias is authored intent even when only its sibling is in the plan.
    ///
    /// The settled lock keeps the alias's own `beta` 2.3.0, so only the duplicate guard, not the
    /// acceptance rule, has a reason to refuse it.
    #[cfg(unix)]
    #[tokio::test]
    async fn duplicate_guard_apply_rejects_inherited_alias_split() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path()).ok_or_else(|| eyre::eyre!("utf8 root"))?;
        let (project, tool) = atomic_fixture(root, "")?;
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {r#"
                [workspace]
                [workspace.dependencies]
                runtime = { package = "beta", version = "2" }
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2024"
                [dependencies]
                alpha = "2"
                runtime.workspace = true
            "#},
        )?;
        std::fs::write(
            root.join("Cargo.lock.settled"),
            indoc! {r#"
                version = 4

                [[package]]
                name = "app"
                version = "0.1.0"
                dependencies = ["alpha", "beta 2.3.0"]

                [[package]]
                name = "alpha"
                version = "2.2.0"
                source = "registry+https://github.com/rust-lang/crates.io-index"
                checksum = "cc"
                dependencies = ["beta 3.0.0"]

                [[package]]
                name = "beta"
                version = "2.3.0"
                source = "registry+https://github.com/rust-lang/crates.io-index"
                checksum = "bb"

                [[package]]
                name = "beta"
                version = "3.0.0"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#},
        )?;
        std::fs::write(
            root.join("metadata.json"),
            indoc! {r#"
                {
                    "packages": [
                        {"id":"app", "name":"app", "version":"0.1.0",
                         "manifest_path":"/repo/Cargo.toml", "dependencies":[
                            {"name":"beta", "rename":"runtime", "req":"^2"},
                            {"name":"alpha", "req":"^2"}]},
                        {"id":"beta", "name":"beta", "version":"2.3.0",
                         "source":"registry+https://github.com/rust-lang/crates.io-index"},
                        {"id":"alpha", "name":"alpha", "version":"2.2.0",
                         "source":"registry+https://github.com/rust-lang/crates.io-index"},
                        {"id":"beta-new", "name":"beta", "version":"3.0.0",
                         "source":"registry+https://github.com/rust-lang/crates.io-index"}
                    ],
                    "workspace_members":["app"], "workspace_root":"/repo",
                    "resolve":{"nodes":[
                        {"id":"app", "deps":[{"name":"runtime", "pkg":"beta"},
                            {"name":"alpha", "pkg":"alpha"}]},
                        {"id":"alpha", "deps":[{"name":"beta", "pkg":"beta-new"}]},
                        {"id":"beta", "deps":[]}, {"id":"beta-new", "deps":[]}
                    ]}
                }
            "#},
        )?;
        std::fs::write(
            root.join("fake-cargo.sh"),
            indoc! {r#"
                #!/bin/sh
                case "$1" in
                  update) cp Cargo.lock.settled Cargo.lock; exit 0 ;;
                  metadata) cp Cargo.lock.settled Cargo.lock; cat metadata.json; exit 0 ;;
                esac
                exit 1
            "#},
        )?;
        let plan = Plan {
            changes: vec![Change {
                direct: false,
                ..change("alpha", "2.3.0", "2.2.0", true)
            }],
            ..Default::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;
        let error = tool
            .apply_plan(&project, &plan, &journal, None)
            .await
            .expect_err("settled authored sibling split must trigger candidate isolation");
        assert!(matches!(error, CoreError::UnacceptableResolve(detail)
            if detail.contains("direct dependency beta") && detail.contains("2.3.0 and 3.0.0")
            && detail.contains("retained by app 0.1.0")
            && detail.contains("keep this candidate held")
            && !detail.contains("keep beta held")));
        assert!(tool.guards_duplicate_copies());
        Ok(())
    }
    // Only [`InPlaceCargoFamilyWriter`] implements a trait here, and it is Unix-only.
    #[cfg(unix)]
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use cooldown_adapter_util::skipped_on_apply_error;
    use cooldown_core::CoreError;
    use indoc::{formatdoc, indoc};

    /// A two-member workspace under a temp root, as `cargo metadata` would describe it, whose
    /// `hack` member's manifest carries the hakari marker and mirrors `dep`; `app` declares `dep`
    /// too, while `orphan` is the hack's alone.
    fn hakari_workspace(root: &Utf8Path) -> ResolvedGraph {
        std::fs::create_dir_all(root.join("crates/app")).expect("app dir");
        std::fs::create_dir_all(root.join("crates/hack")).expect("hack dir");
        std::fs::write(
            root.join("crates/app/Cargo.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = \"1\"\n",
        )
        .expect("app manifest");
        std::fs::write(
            root.join("crates/hack/Cargo.toml"),
            indoc! {r#"
                [package]
                name = "hack"

                ### BEGIN HAKARI SECTION
                [dependencies]
                dep = "1"
                orphan = "2"
                ### END HAKARI SECTION
            "#},
        )
        .expect("hack manifest");
        // A Windows temp root carries backslashes, which are escapes inside a JSON string.
        let root = serde_json::to_string(root.as_str()).expect("json root");
        let root = root.trim_matches('"');
        let json = formatdoc! {r#"{{
            "packages": [
                {{"id": "app", "name": "app", "version": "0.1.0",
                 "manifest_path": "{root}/crates/app/Cargo.toml",
                 "dependencies": [{{"name": "dep", "req": "^1"}}, {{"name": "hack", "req": "^0.1"}}]}},
                {{"id": "hack", "name": "hack", "version": "0.1.0",
                 "manifest_path": "{root}/crates/hack/Cargo.toml",
                 "dependencies": [{{"name": "dep", "req": "^1"}}, {{"name": "orphan", "req": "^2"}}]}},
                {{"id": "dep", "name": "dep", "version": "1.0.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index"}},
                {{"id": "orphan", "name": "orphan", "version": "2.0.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index"}}
            ],
            "workspace_members": ["app", "hack"],
            "workspace_root": "{root}",
            "resolve": {{"nodes": [
                {{"id": "app", "deps": [{{"name": "dep", "pkg": "dep"}}, {{"name": "hack", "pkg": "hack"}}]}},
                {{"id": "hack", "deps": [{{"name": "dep", "pkg": "dep"}}, {{"name": "orphan", "pkg": "orphan"}}]}},
                {{"id": "dep", "deps": []}},
                {{"id": "orphan", "deps": []}}
            ]}}
        }}"#};
        crate::cargocmd::Cargo::build_graph_from_json(&json)
    }

    fn hakari_project(root: &Utf8Path, generated: cooldown_core::GeneratedMembers) -> Project {
        Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: generated,
        }
    }

    /// Without a declaration the marker earns a hint that names the member, counts the
    /// declarations about to be proposed as upgrades, and spells the declaration to make.
    #[test]
    fn undeclared_hakari_member_earns_a_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let graph = hakari_workspace(root);
        let project = hakari_project(root, cooldown_core::GeneratedMembers::undeclared());

        let facts = generated_facts(&project, &graph);

        assert_eq!(facts.members, []);
        let [hint] = facts.notices.as_slice() else {
            panic!("one hint: {:?}", facts.notices);
        };
        assert_eq!(hint.kind, DiagnosticKind::Config);
        assert_eq!(hint.path.as_deref(), Some("crates/hack/Cargo.toml"));
        assert!(hint.message.contains("member `hack`"), "{}", hint.message);
        assert!(
            hint.message.contains("2 dependency declarations"),
            "{}",
            hint.message
        );
        assert!(
            hint.message.contains("generated-members = [\"hack\"]"),
            "{}",
            hint.message
        );
    }

    /// An explicit empty declaration is the user's decision that every member is authored: no
    /// hint, no followers.
    #[test]
    fn explicit_empty_declaration_silences_the_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let graph = hakari_workspace(root);
        let project = hakari_project(root, cooldown_core::GeneratedMembers::declared(Vec::new()));

        let facts = generated_facts(&project, &graph);

        assert_eq!(facts.members, []);
        assert!(facts.notices.is_empty(), "{:?}", facts.notices);
    }

    /// Declared, the member resolves to its manifest for the follower rewrite and the projection's
    /// stale line — a crate nothing authored reaches — is reported for regeneration.
    #[test]
    fn declared_hakari_member_reports_its_stale_projection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        let generated = cooldown_core::GeneratedMembers::declared(vec!["hack".to_string()]);
        let json_graph = hakari_workspace(root);
        // The fixture graph was built undeclared; rebuild it under the declaration the way the
        // adapter's read does.
        drop(json_graph);
        let graph = {
            let manifest = std::fs::read_to_string(root.join("crates/hack/Cargo.toml"))
                .expect("hack manifest");
            assert!(manifest.contains(HAKARI_SECTION_MARKER));
            let mut raw_graph = hakari_workspace(root);
            raw_graph.generated_roots.insert("hack".to_string());
            raw_graph
        };
        let project = hakari_project(root, generated);

        let facts = generated_facts(&project, &graph);

        assert_eq!(
            facts.members,
            vec![MemberRef {
                name: "hack".to_string(),
                path: "crates/hack".to_string()
            }]
        );
        let [stale] = facts.notices.as_slice() else {
            panic!("one staleness notice: {:?}", facts.notices);
        };
        assert_eq!(stale.kind, DiagnosticKind::StaleLock);
        assert_eq!(stale.path.as_deref(), Some("crates/hack/Cargo.toml"));
        assert!(stale.message.contains("`hack`"), "{}", stale.message);
        assert!(stale.message.contains("orphan 2.0.0"), "{}", stale.message);
        assert!(
            !stale.message.contains("dep 1.0.0"),
            "a line an authored member reaches is not stale: {}",
            stale.message
        );
        assert!(stale.message.contains("regenerate"), "{}", stale.message);
    }

    /// A change attributed only to a generated member (a node its projection alone holds) has
    /// no author to widen, so the source-blind key rewrite never touches the projection's git
    /// namesake, and no root fallback runs either; the projection's crates.io line still follows.
    #[test]
    fn widen_and_follow_never_widens_a_generated_member_as_an_author() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("hack")).expect("mkdir");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"hack\"]\n\n[workspace.dependencies]\nfoo = \"1\"\n",
        )
        .expect("root");
        std::fs::write(
            root.join("hack/Cargo.toml"),
            indoc! {r#"
                [dependencies]
                foo = { git = "https://example.com/foo", rev = "abc", version = "1" }
                foo-crates = { package = "foo", version = "1" }
            "#},
        )
        .expect("hack");
        let hack = MemberRef {
            name: "hack".to_string(),
            path: "hack".to_string(),
        };
        let mut planned = change("foo", "1.0.0", "2.0.0", false);
        planned.members = vec![hack.clone()];

        let rewrite = widen_and_follow(
            root,
            &planned,
            std::slice::from_ref(&hack),
            RewriteMode::Auto,
        )
        .expect("rewrite");

        assert_eq!(rewrite.modified, vec![Utf8PathBuf::from("hack/Cargo.toml")]);
        let hack_after = std::fs::read_to_string(root.join("hack/Cargo.toml")).expect("read");
        assert!(
            hack_after.contains(r#"rev = "abc", version = "1""#),
            "{hack_after}"
        );
        assert!(
            hack_after.contains(r#"foo-crates = { package = "foo", version = "2.0.0" }"#),
            "{hack_after}"
        );
        let root_after = std::fs::read_to_string(root.join("Cargo.toml")).expect("read");
        assert!(
            root_after.contains("foo = \"1\""),
            "no root fallback: {root_after}"
        );
    }

    /// A projection-retained node follows to the nearest authored line above it — the line its
    /// requirers moved to — and to the highest when every authored line is below it.
    #[test]
    fn companion_target_picks_the_nearest_authored_line_above() {
        let authored = [
            "0.22.1".to_string(),
            "0.20.0".to_string(),
            "0.19.3".to_string(),
        ];
        assert_eq!(companion_target(&authored, "0.19.15"), Some("0.20.0"));
        assert_eq!(companion_target(&authored, "0.21.0"), Some("0.22.1"));
        assert_eq!(companion_target(&authored, "0.23.0"), Some("0.22.1"));
        assert_eq!(companion_target(&[], "0.19.15"), None);
    }

    #[test]
    fn advisory_ecosystem_matches_osv() {
        let cache = tempfile::tempdir().expect("cache");
        let tool = CargoTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );
        assert_eq!(tool.capabilities().advisory_ecosystem, Some("crates.io"));
    }

    fn lock_with(packages: &[(&str, &str)]) -> CargoLock {
        let sourced: Vec<(&str, &str, &str)> = packages
            .iter()
            .map(|(name, version)| {
                (
                    *name,
                    *version,
                    "registry+https://github.com/rust-lang/crates.io-index",
                )
            })
            .collect();
        lock_with_sources(&sourced)
    }

    fn lock_with_sources(packages: &[(&str, &str, &str)]) -> CargoLock {
        let mut content = String::from("version = 4\n");
        for (name, version, source) in packages {
            content.push_str(&formatdoc! {r#"

                [[package]]
                name = "{name}"
                version = "{version}"
                source = "{source}"
            "#});
        }
        CargoLock::parse(&content).expect("lock parses")
    }

    fn change(name: &str, from: &str, to: &str, downgrade: bool) -> Change {
        Change {
            package: PackageId::new(CARGO_ID, name, Some(CRATES_IO.to_string())),
            from: Version::new(from),
            to: Version::new(to),
            kind: cooldown_core::UpdateKind::Minor,
            downgrade,
            direct: true,
            members: Vec::new(),
        }
    }

    /// A same-family writer that prepares mutations in place, standing in for a rogue adapter.
    ///
    /// Only the isolated execution mode rejects a mutation this produced, so the tests using it
    /// are Unix-only.
    #[cfg(unix)]
    struct InPlaceCargoFamilyWriter;

    #[cfg(unix)]
    #[async_trait]
    impl ToolWrite for InPlaceCargoFamilyWriter {
        fn mutation_tool(&self) -> ToolId {
            CARGO_ID
        }

        async fn mutation_journal(
            &self,
            project: &Project,
            _plan: &Plan,
        ) -> Result<ProjectMutationJournal> {
            ProjectMutationJournal::capture(
                &project.root,
                [Utf8Path::new("Cargo.toml"), Utf8Path::new("Cargo.lock")],
            )
        }

        async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
            mutation.parts_for(self)?;
            Ok(ApplyReport::default())
        }

        async fn build(&self, _project: &Project) -> Result<VerifyReport> {
            Ok(VerifyReport {
                ok: true,
                detail: String::new(),
            })
        }
    }

    #[test]
    fn candidate_metadata_derives_the_same_locked_release_shape() {
        let published_at = "2026-01-02T03:04:05Z".parse().expect("valid timestamp");
        let dep = Dependency {
            package: PackageId::new(CARGO_ID, "serde", Some(CRATES_IO.to_string())),
            advisory_identity: Some("serde".to_string()),
            current: Version::new("1.0.200"),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        };
        let releases = [Release {
            version: dep.current.clone(),
            order: ReleaseOrder(vec![42]),
            major: version::major_key(dep.current.as_str()),
            major_number: version::major_number(dep.current.as_str()),
            kind_from_current: Some(UpdateKind::Patch),
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: Some(published_at),
            yanked: true,
            quality: ReleaseQuality::Prerelease,
        }];

        let locked = derive_locked_release(&dep, &releases).expect("current release is present");

        assert_eq!(locked.version, dep.current);
        assert_eq!(locked.order, ReleaseOrder(Vec::new()));
        assert_eq!(locked.major, version::major_key(dep.current.as_str()));
        assert_eq!(locked.kind_from_current, None);
        assert_eq!(locked.published_at, Some(published_at));
        assert!(!locked.yanked);
        assert_eq!(locked.quality, dep.current_quality);
    }

    #[test]
    fn locked_versions_by_source_skips_non_registry_and_keys_per_major() {
        let lock = CargoLock::parse(indoc! {r#"
            version = 4

            [[package]]
            name = "demo"
            version = "0.1.0"

            [[package]]
            name = "serde"
            version = "1.0.197"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "serde"
            version = "1.0.99"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "serde"
            version = "0.9.15"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "local-git"
            version = "2.0.0"
            source = "git+https://example.com/x#abc"
        "#})
        .expect("lock parses");
        let slots = lock.locked_versions_by_source();
        // The path/workspace member `demo` and the git source are excluded; the two serde majors are
        // distinct slots, and semantic ordering chooses 1.0.197 over lexically-greater 1.0.99.
        let crates_io = "registry+https://github.com/rust-lang/crates.io-index";
        assert_eq!(
            slots
                .get(&(crates_io.into(), "serde".into(), "1".into()))
                .map(String::as_str),
            Some("1.0.197")
        );
        assert_eq!(
            slots
                .get(&(crates_io.into(), "serde".into(), "0.9".into()))
                .map(String::as_str),
            Some("0.9.15")
        );
        assert!(!slots.keys().any(|(_, name, _)| name == "demo"));
        assert!(!slots.keys().any(|(_, name, _)| name == "local-git"));
    }

    #[test]
    fn reached_requires_the_exact_target_in_its_major_slot() {
        let after =
            lock_with(&[("serde", "1.0.200"), ("syn", "2.0.50")]).crates_io_locked_versions();
        // A concrete Cargo `--precise` target must land exactly; an overshoot remains off-policy.
        assert!(reached(
            &after,
            &change("serde", "1.0.100", "1.0.200", false)
        ));
        assert!(!reached(
            &after,
            &change("serde", "1.0.100", "1.0.150", false)
        ));
        // The same exactness applies to downgrades; undershooting the matured target is not success.
        assert!(reached(&after, &change("syn", "2.0.60", "2.0.50", true)));
        assert!(!reached(&after, &change("syn", "2.0.70", "2.0.60", true)));
        // A candidate absent from the lock did not reach its target.
        assert!(!reached(&after, &change("tokio", "1.0.0", "1.5.0", false)));

        let private_target = CargoLock::parse(indoc! {r#"
            version = 4

            [[package]]
            name = "serde"
            version = "1.0.100"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "serde"
            version = "1.0.200"
            source = "registry+https://packages.example.invalid/index"
        "#})
        .expect("mixed-registry lock parses");
        assert!(
            !reached(
                &private_target.crates_io_locked_versions(),
                &change("serde", "1.0.100", "1.0.200", false),
            ),
            "an alternate-registry target does not satisfy a crates.io plan"
        );
    }

    #[test]
    fn apply_graph_is_loaded_to_attribute_a_held_transitive_candidate() {
        let landed = lock_with(&[("shared", "1.1.0")]).crates_io_locked_versions();
        let mut transitive = change("shared", "1.0.0", "1.1.0", false);
        transitive.direct = false;

        assert!(!needs_apply_graph(
            std::slice::from_ref(&transitive),
            &landed
        ));

        let held = lock_with(&[("shared", "1.0.0")]).crates_io_locked_versions();
        assert!(needs_apply_graph(&[transitive], &held));
    }

    #[test]
    fn current_selector_tracks_a_unique_node_moved_by_an_earlier_pin() {
        let planned = change("referencing", "0.46.5", "0.46.6", false);

        let floated = lock_with(&[("referencing", "0.46.10")]);
        assert_eq!(
            current_selector(&floated, &planned).as_deref(),
            Some("0.46.10")
        );

        let landed = lock_with(&[("referencing", "0.46.6")]);
        assert_eq!(current_selector(&landed, &planned), None);
    }

    #[test]
    fn current_selector_keeps_the_original_line_and_rejects_ambiguity() {
        let planned = change("referencing", "0.46.5", "0.46.6", false);
        let original_and_target =
            lock_with(&[("referencing", "0.46.5"), ("referencing", "0.46.6")]);
        assert_eq!(
            current_selector(&original_and_target, &planned).as_deref(),
            Some("0.46.5"),
            "a sibling target must not mask the original planned line"
        );

        let ambiguous = lock_with(&[("referencing", "0.46.7"), ("referencing", "0.46.10")]);
        assert_eq!(
            current_selector(&ambiguous, &planned),
            None,
            "the adapter must not guess between coexisting off-target lines"
        );

        let alternate_registry = CargoLock::parse(indoc! {r#"
            version = 4

            [[package]]
            name = "referencing"
            version = "0.46.10"
            source = "registry+https://packages.example.invalid/index"
        "#})
        .expect("alternate-registry lock parses");
        assert_eq!(
            current_selector(&alternate_registry, &planned),
            None,
            "a private-registry namesake is not the moved crates.io node"
        );
    }

    #[test]
    fn target_gated_workspace_duplicate_requires_member_aware_rewrite() {
        let after = lock_with(&[("nix", "0.28.0"), ("nix", "0.31.3")]).crates_io_locked_versions();
        let graph = crate::cargocmd::Cargo::build_graph_from_json(
            r#"{
                "packages": [
                    {"id": "mcp", "name": "micromux-mcp", "version": "0.1.0",
                     "manifest_path": "/repo/crates/micromux-mcp/Cargo.toml",
                     "dependencies": [{"name": "nix", "req": "^0.28", "target": "cfg(unix)"}]},
                    {"id": "core", "name": "micromux", "version": "0.1.0",
                     "manifest_path": "/repo/crates/micromux/Cargo.toml",
                     "dependencies": [{"name": "nix", "req": "^0.31"}]},
                    {"id": "nix-old", "name": "nix", "version": "0.28.0",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []},
                    {"id": "nix-new", "name": "nix", "version": "0.31.3",
                     "source": "registry+https://github.com/rust-lang/crates.io-index",
                     "dependencies": []}
                ],
                "workspace_members": ["mcp", "core"],
                "workspace_root": "/repo",
                "resolve": {"nodes": [
                    {"id": "mcp", "deps": [{"pkg": "nix-old"}]},
                    {"id": "core", "deps": [{"pkg": "nix-new"}]},
                    {"id": "nix-old", "deps": []},
                    {"id": "nix-new", "deps": []}
                ]}
            }"#,
        );
        let mcp_member = cooldown_core::MemberRef {
            name: "micromux-mcp".to_string(),
            path: "crates/micromux-mcp".to_string(),
        };
        let mut change = change("nix", "0.28.0", "0.31.3", false);
        change.members = vec![mcp_member.clone()];

        assert!(
            reached(&after, &change),
            "the lock has nix 0.31.3 for a different workspace member"
        );
        assert!(
            !reached_after(&after, None, &change),
            "a direct member change must not fall back to the member-blind lock slot"
        );
        assert!(
            !reached_after(&after, Some(&graph), &change),
            "micromux-mcp still resolves nix 0.28.0, so Auto mode must widen its manifest"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::create_dir_all(root.join("crates/micromux-mcp")).expect("mkdir");
        std::fs::write(
            root.join("crates/micromux-mcp/Cargo.toml"),
            indoc! {r#"
                [package]
                name = "micromux-mcp"

                [target.'cfg(unix)'.dependencies]
                nix = { version = "0.28", features = ["signal"] }
            "#},
        )
        .expect("write manifest");

        let rewrite = manifest::widen_constraint(
            &root,
            std::slice::from_ref(&mcp_member),
            "nix",
            "0.31.3",
            RewriteMode::Always,
        )
        .expect("rewrite target-gated dep");

        assert_eq!(
            rewrite.modified,
            vec![Utf8PathBuf::from("crates/micromux-mcp/Cargo.toml")]
        );
        let manifest =
            std::fs::read_to_string(root.join("crates/micromux-mcp/Cargo.toml")).expect("read");
        assert!(manifest.contains(r#"version = "0.31.3""#), "{manifest}");
        assert!(manifest.contains(r#"features = ["signal"]"#), "{manifest}");
    }

    /// A corrective edge rewrite can be the operation that makes a direct member reach its planned
    /// target.
    /// Success classification must therefore consume the graph returned by verification, not the
    /// metadata snapshot taken before enforcement.
    #[test]
    fn member_reach_classification_changes_with_the_verified_edge_graph() {
        let graph = |app_target: &str| {
            let json = formatdoc!(
                r#"{{
                    "packages": [
                        {{"id": "app", "name": "app", "version": "0.1.0",
                         "manifest_path": "/repo/Cargo.toml",
                         "dependencies": [{{"name": "dep", "req": ">=1.0, <2"}}]}},
                        {{"id": "keeper", "name": "keeper", "version": "1.0.0",
                         "dependencies": [{{"name": "dep", "req": "=1.0.0"}}]}},
                        {{"id": "consumer", "name": "consumer", "version": "1.0.0",
                         "dependencies": [{{"name": "dep", "req": "=1.1.0"}}]}},
                        {{"id": "dep-old", "name": "dep", "version": "1.0.0",
                         "source": "registry+https://github.com/rust-lang/crates.io-index",
                         "dependencies": []}},
                        {{"id": "dep-target", "name": "dep", "version": "1.1.0",
                         "source": "registry+https://github.com/rust-lang/crates.io-index",
                         "dependencies": []}}
                    ],
                    "workspace_members": ["app"],
                    "workspace_root": "/repo",
                    "resolve": {{"nodes": [
                        {{"id": "app", "deps": [{{"pkg": "{app_target}"}}, {{"pkg": "keeper"}}, {{"pkg": "consumer"}}]}},
                        {{"id": "keeper", "deps": [{{"pkg": "dep-old"}}]}},
                        {{"id": "consumer", "deps": [{{"pkg": "dep-target"}}]}},
                        {{"id": "dep-old", "deps": []}},
                        {{"id": "dep-target", "deps": []}}
                    ]}}
                }}"#,
            );
            crate::cargocmd::Cargo::build_graph_from_json(&json)
        };
        let stale = graph("dep-old");
        let verified = graph("dep-target");
        let after = lock_with(&[("dep", "1.0.0"), ("dep", "1.1.0")]).crates_io_locked_versions();
        let mut change = change("dep", "1.0.0", "1.1.0", false);
        change.members = vec![cooldown_core::MemberRef {
            name: "app".to_string(),
            path: ".".to_string(),
        }];

        assert!(!reached_after(&after, Some(&stale), &change));
        assert!(reached_after(&after, Some(&verified), &change));
    }

    #[test]
    fn collateral_change_surfaces_a_forced_non_candidate_downgrade() {
        // Raising `a` forces the shared transitive `shared` from 1.1.0 down to 1.0.0 as a consistency
        // move. No applied row reports `shared`, so the diff must surface it as its own collateral
        // row — the silent drift the earlier per-precise-pin design allowed.
        let before = lock_with(&[("a", "1.0.0"), ("shared", "1.1.0")]).locked_versions_by_source();
        let after = lock_with(&[("a", "2.0.0"), ("shared", "1.0.0")]).locked_versions_by_source();
        let applied = [change("a", "1.0.0", "2.0.0", false)];
        let collateral = collateral_changes(&before, &after, &applied, &DeclaredSides::default());
        assert_eq!(collateral.len(), 1);
        let shared = &collateral[0];
        assert_eq!(shared.package.name, "shared");
        assert_eq!(shared.from.as_str(), "1.1.0");
        assert_eq!(shared.to.as_str(), "1.0.0");
        assert!(
            shared.downgrade,
            "a forced regression is reported as a downgrade"
        );
    }

    #[test]
    fn collateral_change_pairs_a_cross_line_companion_float() {
        // The luup5 cranelift case: pinning wasmtime 46 → 47 drags its unplanned cranelift
        // companion from the 0.133 line onto 0.134. Same-line pairing can never see this move —
        // 0.x minors are distinct lines — so it was silently absent from the report, and the later
        // reconcile leg (0.134.3 → 0.134.2) had no first leg to collapse against.
        let before = lock_with(&[("wasmtime", "46.0.1"), ("cranelift-codegen", "0.133.1")])
            .locked_versions_by_source();
        let after = lock_with(&[("wasmtime", "47.0.2"), ("cranelift-codegen", "0.134.3")])
            .locked_versions_by_source();
        let applied = [change("wasmtime", "46.0.1", "47.0.2", false)];
        let collateral = collateral_changes(&before, &after, &applied, &DeclaredSides::default());
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "cranelift-codegen");
        assert_eq!(collateral[0].from.as_str(), "0.133.1");
        assert_eq!(collateral[0].to.as_str(), "0.134.3");
        assert!(!collateral[0].downgrade);
    }

    #[test]
    fn collateral_change_zips_coexisting_lines_by_rank() {
        // Two coexisting wasmparser lines each advance one step in the same re-lock; rank order
        // pairs each old line with its successor instead of cross-wiring them.
        let before = lock_with(&[("wasmparser", "0.251.0"), ("wasmparser", "0.253.0")])
            .locked_versions_by_source();
        let after = lock_with(&[("wasmparser", "0.252.0"), ("wasmparser", "0.254.0")])
            .locked_versions_by_source();
        let collateral = collateral_changes(&before, &after, &[], &DeclaredSides::default());
        let moves: Vec<(&str, &str)> = collateral
            .iter()
            .map(|change| (change.from.as_str(), change.to.as_str()))
            .collect();
        assert_eq!(moves, [("0.251.0", "0.252.0"), ("0.253.0", "0.254.0")]);
    }

    #[test]
    fn collateral_change_falls_back_to_same_line_pairing_when_a_line_appears() {
        // `dep` forks a new major (2.0.0 appears) while its 1.x line also patches: unequal residual
        // counts make rank pairing ambiguous, so only the same-line 1.x move is reported.
        let before = lock_with(&[("dep", "1.0.0")]).locked_versions_by_source();
        let after = lock_with(&[("dep", "1.0.1"), ("dep", "2.0.0")]).locked_versions_by_source();
        let collateral = collateral_changes(&before, &after, &[], &DeclaredSides::default());
        let moves: Vec<(&str, &str)> = collateral
            .iter()
            .map(|change| (change.from.as_str(), change.to.as_str()))
            .collect();
        assert_eq!(moves, [("1.0.0", "1.0.1")]);
    }

    #[test]
    fn collateral_change_excludes_applied_and_unchanged_packages() {
        // `a`'s move is already told by its applied row (no duplicate), `b` is unchanged (no row),
        // `c` is an unplanned forward move (a real collateral change). Only `c` is surfaced.
        let before = lock_with(&[("a", "2.0.0"), ("b", "2.0.0"), ("c", "1.0.0")])
            .locked_versions_by_source();
        let after = lock_with(&[("a", "1.0.0"), ("b", "2.0.0"), ("c", "1.5.0")])
            .locked_versions_by_source();
        let applied = [change("a", "2.0.0", "1.0.0", true)];
        let collateral = collateral_changes(&before, &after, &applied, &DeclaredSides::default());
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "c");
        assert!(!collateral[0].downgrade);
    }

    #[test]
    fn collateral_change_keeps_registries_apart() {
        // A crates.io crate and an alternate-registry crate share a name and major line. Slots are
        // compared per source, so the alternate registry's move neither pairs against the
        // crates.io baseline (which would fabricate endpoints) nor gets labeled crates.io.
        let crates_io = "registry+https://github.com/rust-lang/crates.io-index";
        let alt = "registry+sparse+https://registry.example.com/index/";
        let before = lock_with_sources(&[("dep", "1.0.0", crates_io), ("dep", "1.2.0", alt)])
            .locked_versions_by_source();
        let after = lock_with_sources(&[("dep", "1.0.0", crates_io), ("dep", "1.3.0", alt)])
            .locked_versions_by_source();
        let collateral = collateral_changes(&before, &after, &[], &DeclaredSides::default());
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].from.as_str(), "1.2.0");
        assert_eq!(collateral[0].to.as_str(), "1.3.0");
        assert_eq!(
            collateral[0].package.registry.as_deref(),
            Some("registry:registry.example.com"),
            "the row names its own registry, not crates.io"
        );
    }

    /// A collateral row is direct exactly when an authored package declares the node it leaves or
    /// reaches, matched by exact identity rather than by name.
    #[test]
    fn collateral_change_is_direct_for_a_declared_node() -> eyre::Result<()> {
        let lock = |declared: &str, syn_one: &str, syn_two: &str| {
            CargoLock::parse(&formatdoc! {r#"
                version = 4

                [[package]]
                name = "app"
                version = "0.1.0"
                dependencies = ["syn {declared}"]

                [[package]]
                name = "syn"
                version = "{syn_one}"
                source = "registry+https://github.com/rust-lang/crates.io-index"

                [[package]]
                name = "syn"
                version = "{syn_two}"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#})
        };
        let before = lock("1.0.100", "1.0.100", "2.0.50")?;
        let after = lock("1.0.109", "1.0.109", "2.0.60")?;
        let generated = cooldown_core::GeneratedMembers::undeclared();
        let declared = DeclaredSides {
            before: before.declared_endpoints(&generated),
            after: after.declared_endpoints(&generated),
        };

        let collateral = collateral_changes(
            &before.locked_versions_by_source(),
            &after.locked_versions_by_source(),
            &[],
            &declared,
        );

        assert_eq!(
            collateral,
            [
                collateral_change(CRATES_IO, "syn", "1.0.100", "1.0.109", true),
                collateral_change(CRATES_IO, "syn", "2.0.50", "2.0.60", false),
            ]
        );
        Ok(())
    }

    #[test]
    fn collateral_changes_surface_a_held_candidates_real_movement() {
        // A held planned candidate has no applied row, yet a sibling pin still floated it off its
        // baseline. That net move must surface as a collateral row beside the held skip instead of
        // being silently dropped behind the planned name.
        let before = lock_with(&[("referencing", "0.46.5")]).locked_versions_by_source();
        let after = lock_with(&[("referencing", "0.46.10")]).locked_versions_by_source();
        let collateral = collateral_changes(&before, &after, &[], &DeclaredSides::default());
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "referencing");
        assert_eq!(collateral[0].from.as_str(), "0.46.5");
        assert_eq!(collateral[0].to.as_str(), "0.46.10");

        // Once the candidate reaches its target, its applied row already tells the move: no
        // duplicate collateral row.
        let landed = lock_with(&[("referencing", "0.46.10")]).locked_versions_by_source();
        let applied = [change("referencing", "0.46.5", "0.46.10", false)];
        assert_eq!(
            collateral_changes(&before, &landed, &applied, &DeclaredSides::default()),
            []
        );
    }

    #[test]
    fn summarize_pin_rejection_names_the_requirement_and_requirer() {
        // Verbatim (trimmed) stderr shape from a live `cargo update -p regex --precise 1.13.1`
        // rejection: the blocking requirement plus the first requirer make the report row
        // actionable where "the resolver rejected this change" was not.
        let stderr = indoc::indoc! {r#"
            error: failed to select a version for the requirement `regex = ">=1.0, <1.13"`
            candidate versions found which didn't match: 1.13.1
            location searched: crates.io index
            required by package `serde-saphyr v0.0.16`
                ... which satisfies dependency `serde-saphyr = "^0.0.16"` (locked to 0.0.16) of package `mistralrs-core v0.9.0 (https://github.com/EricLBuehler/mistral.rs.git?tag=v0.9.0#54957525)`
        "#};
        let error = cooldown_core::CoreError::Tool {
            tool: "cargo".into(),
            termination: cooldown_core::ToolTermination::ExitCode(101),
            stderr: stderr.into(),
        };
        assert_eq!(
            summarize_pin_rejection(&error).as_deref(),
            Some(
                "failed to select a version for the requirement `regex = \">=1.0, <1.13\"` \
                 (required by serde-saphyr v0.0.16)"
            ),
        );
    }

    #[test]
    fn summarize_pin_rejection_drops_workspace_path_noise_from_the_requirer() {
        // A workspace member requirer carries its path suffix; the report only needs the name.
        let stderr = indoc::indoc! {r#"
            error: failed to select a version for the requirement `bincode = "^2"`
            candidate versions found which didn't match: 3.0.0
            required by package `y-airtype-core v0.0.20 (/home/user/repo/services/collab/y-airtype-core)`
        "#};
        let error = cooldown_core::CoreError::Tool {
            tool: "cargo".into(),
            termination: cooldown_core::ToolTermination::ExitCode(101),
            stderr: stderr.into(),
        };
        assert_eq!(
            summarize_pin_rejection(&error).as_deref(),
            Some(
                "failed to select a version for the requirement `bincode = \"^2\"` \
                 (required by y-airtype-core v0.0.20)"
            ),
        );
    }

    #[test]
    fn summarize_pin_rejection_reads_the_chain_form_attribution() {
        // The unsatisfiable-fork shape: no `the requirement` clause, and the attribution arrives as
        // a `... required by package` chain continuation rather than the bare form.
        let stderr = indoc::indoc! {r"
            Updating crates.io index
            error: failed to select a version for `bincode`.
                ... required by package `y-airtype-core v0.0.20 (/tmp/tree/services/collab/y-airtype-core)`
                ... which satisfies path dependency `y-airtype-core` (locked to 0.0.20) of package `y-airtype v0.0.20 (/tmp/tree/services/collab/y-airtype)`
            versions that meet the requirements `^3.0.0` are: 3.0.0
        "};
        let error = cooldown_core::CoreError::Tool {
            tool: "cargo".into(),
            termination: cooldown_core::ToolTermination::ExitCode(101),
            stderr: stderr.into(),
        };
        assert_eq!(
            summarize_pin_rejection(&error).as_deref(),
            Some("failed to select a version for `bincode` (required by y-airtype-core v0.0.20)"),
        );
    }

    #[test]
    fn summarize_pin_rejection_without_stderr_stays_generic() {
        assert_eq!(
            summarize_pin_rejection(&cooldown_core::CoreError::StaleLock("x".into())),
            None,
        );
    }

    #[test]
    fn blocking_requirer_names_the_exact_pin_holder() {
        // `a` pins `shared =1.0.0` and resolves an edge to it; raising `shared` past 1.0.0 is held by
        // `a`. The graph names `a` as the structural blocker so the held skip can say "conflicts with a".
        let json = r#"{
            "packages": [
                {"id": "root", "name": "root", "version": "0.1.0", "dependencies": []},
                {"id": "a", "name": "a", "version": "1.0.0",
                 "dependencies": [{"name": "shared", "req": "=1.0.0"}]},
                {"id": "shared", "name": "shared", "version": "1.0.0", "dependencies": []}
            ],
            "workspace_members": ["root"],
            "workspace_root": "",
            "resolve": {"nodes": [
                {"id": "root", "deps": [{"pkg": "a"}]},
                {"id": "a", "deps": [{"pkg": "shared"}]},
                {"id": "shared", "deps": []}
            ]}
        }"#;
        let graph = crate::cargocmd::Cargo::build_graph_from_json(json);
        assert_eq!(
            blocking_requirer(&graph, "shared", "1.1.0"),
            Some("a".to_string())
        );
        // A target within the pin (1.0.0) is not held: no blocker.
        assert_eq!(blocking_requirer(&graph, "shared", "1.0.0"), None);
        // A crate no requirer caps yields no blocker.
        assert_eq!(blocking_requirer(&graph, "unrelated", "9.9.9"), None);
    }

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(CARGO_ID, "serde", Some(CRATES_IO.to_string())),
            from: Version::new("1.0.0"),
            to: Version::new("1.0.1"),
            kind: cooldown_core::UpdateKind::Patch,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        let err = CoreError::ToolSpawn {
            tool: "cargo".into(),
            detail: "spawn failed".into(),
        };

        let result = skipped_on_apply_error(&change, err);
        std::assert_matches!(result, Err(CoreError::ToolSpawn { .. }));
    }

    /// The family the adapter declares is `CARGO_MANIFEST`, the one recovery names when it
    /// re-takes the lease a run held (`publication.rs` tests recovery itself), and cargo's apply
    /// rewrites files only, so it never takes the environment turn.
    #[test]
    fn the_lease_family_is_the_declared_manifest() -> eyre::Result<()> {
        let cache = tempfile::tempdir()?;
        let tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let project = Project {
            root: "/repo".into(),
            kind: CARGO_ID,
            manifest: "/repo/Cargo.toml".into(),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };

        assert_eq!(
            tool.lease_family(&project),
            cooldown_core::fs::ManifestFamily::named(crate::CARGO_MANIFEST)
        );
        assert!(!tool.mutation_installs());
        Ok(())
    }

    /// A project Cargo did not stage itself cannot be prepared for mutation.
    ///
    /// The enforcement lives in the isolated execution mode, which Cargo only selects where the
    /// platform grants recovery authority — hence `#[cfg(unix)]`.
    /// Elsewhere Cargo runs the same in-place trial every other ecosystem uses, and a
    /// directly-prepared mutation is exactly what that mode expects.
    #[cfg(unix)]
    #[tokio::test]
    async fn direct_cargo_preparation_requires_an_isolated_project() -> eyre::Result<()> {
        let other = tempfile::tempdir()?;
        let other_root = Utf8PathBuf::from_path_buf(other.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(other_root.join("Cargo.toml"), "[workspace]\n")?;
        let other_project = Project {
            root: other_root.clone(),
            kind: CARGO_ID,
            manifest: other_root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let cache = tempfile::tempdir()?;
        let tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let result = PreparedMutation::prepare(&tool, &other_project, &Plan::default()).await;

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_edge_normalization_requires_an_isolated_project() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n")?;
        let project = Project {
            root: root.clone(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let cache = tempfile::tempdir()?;
        let tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let mutation =
            PreparedMutation::prepare(&InPlaceCargoFamilyWriter, &project, &Plan::default())
                .await?;

        let result = tool
            .normalize_lock_edges(&mutation, EdgePolicy::None, None, &[])
            .await;

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn direct_cargo_apply_rejects_an_in_place_tool_family_capability() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("Cargo.toml"),
            indoc! {r#"
                [package]
                name = "demo"
                version = "0.1.0"
                edition = "2024"

                [dependencies]
                serde = "1"
            "#},
        )?;
        let project = Project {
            root: root.clone(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let cache = tempfile::tempdir()?;
        let tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let plan = Plan {
            changes: vec![change("serde", "1.0.0", "1.0.1", false)],
            ..Plan::default()
        };
        let mutation =
            PreparedMutation::prepare(&InPlaceCargoFamilyWriter, &project, &plan).await?;

        let result = tool.apply(&mutation).await;

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        Ok(())
    }

    #[tokio::test]
    async fn mutation_journal_restore_removes_lock_created_after_capture() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let manifest = root.join("Cargo.toml");
        std::fs::write(
            &manifest,
            indoc! {r#"
                [package]
                name = "demo"
                version = "0.1.0"
                edition = "2024"
            "#},
        )?;
        let cache_dir = tempfile::tempdir()?;
        let eco = CargoTool::from_http(cooldown_registry::SharedHttp::new(
            cache_dir.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let project = Project {
            root: root.clone(),
            kind: CARGO_ID,
            manifest,
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };

        let journal = eco.mutation_journal(&project, &Plan::default()).await?;
        let lock = root.join("Cargo.lock");
        std::fs::write(&lock, "generated")?;

        journal.restore()?;
        assert!(!lock.exists());
        Ok(())
    }

    /// The manifest the tentative-Auto fixture starts from: `dep = "1"` caps the planned
    /// cross-major target, so the pin batch must fail first and the Auto loop must widen.
    #[cfg(unix)]
    const WIDEN_MANIFEST: &str = indoc! {r#"
        [package]
        name = "app"
        version = "0.1.0"
        edition = "2024"

        [dependencies]
        dep = "1"
    "#};

    #[cfg(unix)]
    const WIDEN_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = ["dep", "holder"]

        [[package]]
        name = "dep"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "holder"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        dependencies = ["dep"]
    "#};

    /// A tentative-Auto fixture project driven by a scripted `cargo` stand-in. The script rejects
    /// every `update --precise` while the manifest still declares `dep = "1"` (a real pin cannot
    /// land outside the requirement); with `accept_when_widened` it models the third-party-held
    /// old major staying put by rewriting the lock to *fork* dep 2.0.0 beside the 1.0.0 line once
    /// the widen removed the cap — without it, the pin stays rejected even after widening.
    #[cfg(unix)]
    fn widen_fixture(
        root: &camino::Utf8Path,
        accept_when_widened: bool,
    ) -> eyre::Result<(Project, CargoTool)> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(root.join("Cargo.toml"), WIDEN_MANIFEST)?;
        std::fs::write(root.join("Cargo.lock"), WIDEN_LOCK)?;
        let accept = if accept_when_widened {
            // The widened `app` binds the forked 2.0.0, so its entry names the version.
            std::fs::write(
                root.join("Cargo.lock.forked"),
                indoc! {r#"
                    version = 4

                    [[package]]
                    name = "app"
                    version = "0.1.0"
                    dependencies = ["dep 2.0.0", "holder"]

                    [[package]]
                    name = "dep"
                    version = "1.0.0"
                    source = "registry+https://github.com/rust-lang/crates.io-index"

                    [[package]]
                    name = "dep"
                    version = "2.0.0"
                    source = "registry+https://github.com/rust-lang/crates.io-index"

                    [[package]]
                    name = "holder"
                    version = "1.0.0"
                    source = "registry+https://github.com/rust-lang/crates.io-index"
                    dependencies = ["dep 1.0.0"]
                "#},
            )?;
            indoc! {r#"
                if ! grep -q 'dep = "1"' Cargo.toml; then
                  cp Cargo.lock.forked Cargo.lock
                  if [ "$1" = metadata ]; then
                    printf '%s\n' '{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}'
                  fi
                  exit 0
                fi
            "#}
        } else {
            ""
        };
        let script = root.join("fake-cargo.sh");
        std::fs::write(
            &script,
            formatdoc! {r#"
                #!/bin/sh
                {accept}
                echo 'error: failed to select a version for the requirement `dep = "^1"`' >&2
                echo 'required by package `app v0.1.0 (/repo/app)`' >&2
                exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let cache = tempfile::tempdir()?;
        let mut tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cargo = Cargo::with_bin(script.as_str());
        let project = Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok((project, tool))
    }

    #[cfg(unix)]
    const ATOMIC_MANIFEST: &str = indoc! {r#"
        [package]
        name = "app"
        version = "0.1.0"
        edition = "2024"

        [dependencies]
        alpha = "2"
        beta = "2"
    "#};

    /// A two-member tilde family at its fresh era: the shape whose per-package pins all reject.
    #[cfg(unix)]
    const ATOMIC_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = ["alpha", "beta"]

        [[package]]
        name = "alpha"
        version = "2.3.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "aa"

        [[package]]
        name = "beta"
        version = "2.3.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "bb"
    "#};

    /// The reconciled lock the scripted `cargo metadata` writes when the atomic seed satisfies it:
    /// both family members at their matured targets.
    #[cfg(unix)]
    const ATOMIC_LOCK_RECONCILED: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = ["alpha", "beta"]

        [[package]]
        name = "alpha"
        version = "2.2.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "cc"

        [[package]]
        name = "beta"
        version = "2.2.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "dd"
    "#};

    /// An atomic-fallback fixture driven by a scripted `cargo` stand-in: every `update --precise`
    /// is rejected (the mutually-held tilde family), and `metadata` — the fallback's reconcile —
    /// behaves per `metadata_behavior`, a shell fragment run with the project root as cwd (it
    /// must write `Cargo.lock` and print metadata JSON on success). The script drops a
    /// `metadata-ran` marker so tests can assert whether the reconcile was reached.
    #[cfg(unix)]
    fn atomic_fixture(
        root: &Utf8Path,
        metadata_behavior: &str,
    ) -> eyre::Result<(Project, CargoTool)> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(root.join("Cargo.toml"), ATOMIC_MANIFEST)?;
        std::fs::write(root.join("Cargo.lock"), ATOMIC_LOCK)?;
        std::fs::write(root.join("Cargo.lock.reconciled"), ATOMIC_LOCK_RECONCILED)?;
        std::fs::write(
            root.join("metadata.json"),
            r#"{"packages": [], "workspace_members": [], "workspace_root": "", "resolve": null}"#,
        )?;
        let script = root.join("fake-cargo.sh");
        std::fs::write(
            &script,
            formatdoc! {r#"
                #!/bin/sh
                case "$1" in
                  metadata)
                    touch metadata-ran
                    {metadata_behavior}
                    ;;
                esac
                echo 'error: failed to select a version for the requirement `beta = "~2.3.0"`' >&2
                echo 'required by package `alpha v2.3.0`' >&2
                exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let cache = tempfile::tempdir()?;
        let mut tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cargo = Cargo::with_bin(script.as_str());
        let project = Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok((project, tool))
    }

    #[cfg(unix)]
    fn atomic_plan() -> Plan {
        Plan {
            changes: vec![
                change("alpha", "2.3.0", "2.2.0", true),
                change("beta", "2.3.0", "2.2.0", true),
            ],
            ..Plan::default()
        }
    }

    /// The icu shape end-to-end in miniature: every pin rejected, then the atomic seed lands both
    /// members through one reconcile — rejections cleared, reconciled lock kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_fallback_lands_a_mutually_held_group() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = atomic_fixture(
            root,
            indoc! {"
                cp Cargo.lock.reconciled Cargo.lock
                cat metadata.json
                exit 0
            "},
        )?;
        let plan = atomic_plan();
        let journal = tool.mutation_journal(&project, &plan).await?;

        let rejections = tool
            .whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        assert!(
            rejections.is_empty(),
            "landed members must not keep stale rejections: {rejections:?}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            ATOMIC_LOCK_RECONCILED,
            "the reconciled lock is kept"
        );
        Ok(())
    }

    /// A reconcile the seed cannot satisfy restores the lock byte-for-byte and keeps every
    /// member's recorded per-pin rejection.
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_fallback_restores_the_lock_when_the_reconcile_fails() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        // The metadata case falls through to the script's rejection tail (resolver-style error).
        let (project, tool) = atomic_fixture(root, "")?;
        let plan = atomic_plan();
        let journal = tool.mutation_journal(&project, &plan).await?;

        let rejections = tool
            .whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        assert!(
            root.join("metadata-ran").exists(),
            "the reconcile was attempted"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            ATOMIC_LOCK,
            "a failed reconcile restores the pre-attempt lock byte-for-byte"
        );
        assert_eq!(rejections.len(), 2, "both members keep their rejections");
        Ok(())
    }

    /// All-or-nothing: a reconcile that lands one member while the other's slot ends somewhere
    /// else entirely is restored whole — keeping it would commit an unplanned version for the
    /// missed member.
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_fallback_restores_a_partial_landing() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = atomic_fixture(
            root,
            indoc! {"
                cp Cargo.lock.partial Cargo.lock
                cat metadata.json
                exit 0
            "},
        )?;
        // alpha lands at its 2.2.0 target, but beta's slot ends at an unplanned 9.9.9 — the
        // live-index preference a real unanchored reconcile can produce.
        let partial = ATOMIC_LOCK_RECONCILED.replace(
            "name = \"beta\"\nversion = \"2.2.0\"",
            "name = \"beta\"\nversion = \"9.9.9\"",
        );
        assert_ne!(
            partial, ATOMIC_LOCK_RECONCILED,
            "the partial fixture must differ"
        );
        std::fs::write(root.join("Cargo.lock.partial"), partial)?;
        let plan = atomic_plan();
        let journal = tool.mutation_journal(&project, &plan).await?;

        let mut rejections: PinRejections = plan
            .changes
            .iter()
            .map(|change| (rejection_key(change), "held".to_string()))
            .collect();
        tool.land_rejected_group_atomically(
            &project,
            &plan,
            &journal,
            None,
            &mut rejections,
            &mut MemoStats::default(),
        )
        .await?;

        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            ATOMIC_LOCK,
            "a partial landing is restored whole"
        );
        assert_eq!(rejections.len(), 2, "both members keep their rejections");
        Ok(())
    }

    /// A lone rejection never triggers the fallback: its pin already expressed exactly this move,
    /// so there is no atomicity to gain and no reconcile to risk.
    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_fallback_abstains_for_a_single_rejection() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = atomic_fixture(
            root,
            indoc! {"
                cp Cargo.lock.reconciled Cargo.lock
                cat metadata.json
                exit 0
            "},
        )?;
        let plan = Plan {
            changes: vec![change("alpha", "2.3.0", "2.2.0", true)],
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        let mut rejections = PinRejections::from([(
            rejection_key(&plan.changes[0]),
            "the precise pin was rejected".to_string(),
        )]);
        tool.land_rejected_group_atomically(
            &project,
            &plan,
            &journal,
            None,
            &mut rejections,
            &mut MemoStats::default(),
        )
        .await?;

        assert!(
            !root.join("metadata-ran").exists(),
            "atomic rejection recovery abstains for a single rejection"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            ATOMIC_LOCK,
            "the lock stays untouched"
        );
        assert_eq!(rejections.len(), 1, "the rejection is kept");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tentative_auto_widen_is_restored_when_the_pin_still_cannot_land() -> eyre::Result<()> {
        // The luup5 widen-poison regression: a third-party crate holds the old major, so the
        // widened requirement can never re-lock. The widen must be rolled back byte-identically —
        // a manifest demanding a version the lock does not carry poisons the whole batch at
        // `--locked` verification — and the candidate must keep cargo's own rejection sentence.
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = widen_fixture(root, false)?;
        let plan = Plan {
            changes: vec![change("dep", "1.0.0", "2.0.0", false)],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        let rejections = tool
            .whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.toml"))?,
            WIDEN_MANIFEST,
            "the unlandable widen is restored byte-identically"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            WIDEN_LOCK,
            "a rejected pin leaves the lock untouched"
        );
        let detail = rejections
            .get(&("dep".to_string(), "1.0.0".to_string(), "2.0.0".to_string()))
            .expect("the held candidate keeps its planned-change-keyed rejection");
        assert!(
            detail.contains("failed to select a version") && detail.contains("required by app"),
            "cargo's own sentence and requirer survive: {detail}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tentative_auto_widen_is_restored_when_the_pin_splits_the_declared_line()
    -> eyre::Result<()> {
        // Once the widen lifts the declared cap, the re-pin forks the new major beside the
        // third-party-held old line.
        // The fork splits a crate the workspace declares, which the final guard refuses for the
        // whole apply, so the step itself is refused and only this candidate is held.
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = widen_fixture(root, true)?;
        let plan = Plan {
            changes: vec![change("dep", "1.0.0", "2.0.0", false)],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        let rejections = tool
            .whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        // The widen and the forked lock are restored byte for byte.
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.toml"))?,
            WIDEN_MANIFEST
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            WIDEN_LOCK
        );
        // The held candidate names the split it would have caused.
        let detail = rejections
            .get(&("dep".to_string(), "1.0.0".to_string(), "2.0.0".to_string()))
            .ok_or_else(|| eyre::eyre!("the held candidate keeps its rejection"))?;
        assert!(
            detail.contains("would resolve direct dependency dep at another incompatible line"),
            "{detail}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_widen_repins_a_noop_widen_sibling_after_another_widen_lands() -> eyre::Result<()>
    {
        // A lock-step pair: `lock-a` needs a cross-major widen, while `lock-b`'s target is a
        // transitive move nothing in the manifest declares, so its widen is a no-op. The fake
        // cargo rejects every pin while `lock-a` is unwidened (the shared blocker) and accepts
        // both once the widen landed. `lock-b` must be re-pinned after its sibling's widen+pin
        // progresses — not left held on the stale rejection recorded before the widen.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let manifest = indoc! {r#"
            [package]
            name = "app"
            version = "0.1.0"
            edition = "2024"

            [dependencies]
            lock-a = "1"
        "#};
        std::fs::write(root.join("Cargo.toml"), manifest)?;
        let lock = |a: &str, b: &str| {
            formatdoc! {r#"
                version = 4

                [[package]]
                name = "app"
                version = "0.1.0"
                dependencies = ["lock-a", "lock-b"]

                [[package]]
                name = "lock-a"
                version = "{a}"
                source = "registry+https://github.com/rust-lang/crates.io-index"

                [[package]]
                name = "lock-b"
                version = "{b}"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#}
        };
        std::fs::write(root.join("Cargo.lock"), lock("1.0.0", "1.0.0"))?;
        std::fs::write(root.join("Cargo.lock.a-landed"), lock("2.0.0", "1.0.0"))?;
        std::fs::write(root.join("Cargo.lock.b-landed"), lock("2.0.0", "1.2.0"))?;
        let script = root.join("fake-cargo.sh");
        std::fs::write(
            &script,
            indoc! {r##"
                #!/bin/sh
                if ! grep -q 'lock-a = "1"' Cargo.toml; then
                  case "$*" in
                    *"#lock-a@"*) cp Cargo.lock.a-landed Cargo.lock; exit 0 ;;
                    *"#lock-b@"*) cp Cargo.lock.b-landed Cargo.lock; exit 0 ;;
                  esac
                fi
                echo 'error: failed to select a version for the requirement `lock-a = "^1"`' >&2
                echo 'required by package `app v0.1.0 (/repo/app)`' >&2
                exit 1
            "##},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let cache = tempfile::tempdir()?;
        let mut tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cargo = Cargo::with_bin(script.as_str());
        let project = Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        let plan = Plan {
            changes: vec![
                Change {
                    direct: false,
                    ..change("lock-b", "1.0.0", "1.2.0", false)
                },
                change("lock-a", "1.0.0", "2.0.0", false),
            ],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        tool.whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        let landed = read_lock(&project)?.crates_io_locked_versions();
        assert_eq!(
            landed
                .get(&("lock-a".into(), "2".into()))
                .map(String::as_str),
            Some("2.0.0"),
            "the widened sibling landed at its cross-major target"
        );
        assert_eq!(
            landed
                .get(&("lock-b".into(), "1".into()))
                .map(String::as_str),
            Some("1.2.0"),
            "the no-op-widen candidate is re-pinned once the sibling's widen removed the blocker"
        );
        Ok(())
    }

    /// The manifest the member-candidate fixture starts from: the workspace member `app` itself
    /// declares `dep = "1"`, capping the planned cross-major target so the Auto loop must widen.
    #[cfg(unix)]
    const MEMBER_WIDEN_MANIFEST: &str = indoc! {r#"
        [package]
        name = "app"
        version = "0.1.0"
        edition = "2024"

        [dependencies]
        dep = "1"
        holder = "1"
    "#};

    #[cfg(unix)]
    const MEMBER_WIDEN_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = ["dep", "holder"]

        [[package]]
        name = "dep"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "holder"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
    "#};

    /// What the fixture's `metadata` probe reports while the manifest still declares `dep = "1"`:
    /// the member `app` resolves the old major, so the reach check sends the candidate into the
    /// widen. The member paths only need to be mutually consistent (`/repo` + `/repo/Cargo.toml`
    /// relativize to the `.` member): `metadata` parsing never touches the real filesystem.
    #[cfg(unix)]
    const MEMBER_WIDEN_METADATA_UNWIDENED: &str = indoc! {r#"
        {
            "packages": [
                {"id": "app", "name": "app", "version": "0.1.0",
                 "manifest_path": "/repo/Cargo.toml",
                 "dependencies": [
                    {"name": "dep", "req": "^1"},
                    {"name": "holder", "req": "^1"}
                 ]},
                {"id": "holder", "name": "holder", "version": "1.0.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index",
                 "dependencies": [{"name": "dep", "req": "^1"}]},
                {"id": "dep-old", "name": "dep", "version": "1.0.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index",
                 "dependencies": []}
            ],
            "workspace_members": ["app"],
            "workspace_root": "/repo",
            "resolve": {"nodes": [
                {"id": "app", "deps": [
                    {"name": "dep", "pkg": "dep-old"},
                    {"name": "holder", "pkg": "holder"}
                ]},
                {"id": "holder", "deps": [{"name": "dep", "pkg": "dep-old"}]},
                {"id": "dep-old", "deps": []}
            ]}
        }
    "#};

    /// What the probe reports after its own fork: `app` resolves dep 2.1.0 — past the 2.0.0
    /// target, so the member-aware reach check must fail — while `holder` keeps the 1.0.0 line.
    #[cfg(unix)]
    const MEMBER_WIDEN_METADATA_WIDENED: &str = indoc! {r#"
        {
            "packages": [
                {"id": "app", "name": "app", "version": "0.1.0",
                 "manifest_path": "/repo/Cargo.toml",
                 "dependencies": [
                    {"name": "dep", "req": "^2.0.0"},
                    {"name": "holder", "req": "^1"}
                 ]},
                {"id": "holder", "name": "holder", "version": "1.0.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index",
                 "dependencies": [{"name": "dep", "req": "^1"}]},
                {"id": "dep-old", "name": "dep", "version": "1.0.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index",
                 "dependencies": []},
                {"id": "dep-new", "name": "dep", "version": "2.1.0",
                 "source": "registry+https://github.com/rust-lang/crates.io-index",
                 "dependencies": []}
            ],
            "workspace_members": ["app"],
            "workspace_root": "/repo",
            "resolve": {"nodes": [
                {"id": "app", "deps": [
                    {"name": "dep", "pkg": "dep-new"},
                    {"name": "holder", "pkg": "holder"}
                ]},
                {"id": "holder", "deps": [{"name": "dep", "pkg": "dep-old"}]},
                {"id": "dep-old", "deps": []},
                {"id": "dep-new", "deps": []}
            ]}
        }
    "#};

    /// A member-candidate tentative-Auto fixture (`needs_member_graph`, unlike [`widen_fixture`]'s
    /// member-less candidates): workspace member `app` directly declares `dep = "1"` while
    /// third-party `holder` keeps the old major locked. The scripted cargo rejects every
    /// `update --precise`; its *unlocked* `metadata` probe, once the widen removed the manifest
    /// cap, re-locks the widened requirement by forking dep 2.1.0 — the newest version the widened
    /// caret admits, past the 2.0.0 cooldown target — beside the held 1.0.0 line. With
    /// `probe_errs` the same forking probe then exits nonzero, exercising the probe-error rollback
    /// branch instead of the reach-check one.
    #[cfg(unix)]
    fn member_widen_fixture(
        root: &camino::Utf8Path,
        probe_errs: bool,
    ) -> eyre::Result<(Project, CargoTool)> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(root.join("Cargo.toml"), MEMBER_WIDEN_MANIFEST)?;
        std::fs::write(root.join("Cargo.lock"), MEMBER_WIDEN_LOCK)?;
        std::fs::write(root.join("Cargo.lock.unwidened"), MEMBER_WIDEN_LOCK)?;
        std::fs::write(
            root.join("Cargo.lock.forked"),
            formatdoc! {r#"
                {MEMBER_WIDEN_LOCK}
                [[package]]
                name = "dep"
                version = "2.1.0"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#},
        )?;
        std::fs::write(
            root.join("metadata.unwidened.json"),
            MEMBER_WIDEN_METADATA_UNWIDENED,
        )?;
        std::fs::write(
            root.join("metadata.widened.json"),
            MEMBER_WIDEN_METADATA_WIDENED,
        )?;
        // The forking probe either reports the graph its own fork produced or, in the
        // probe-error variant, leaves the forked lock behind and fails like a resolver conflict.
        let widened_probe = if probe_errs {
            ""
        } else {
            indoc! {"
                cat metadata.widened.json
                exit 0
            "}
        };
        let script = root.join("fake-cargo.sh");
        std::fs::write(
            &script,
            formatdoc! {r#"
                #!/bin/sh
                if [ "$1" = metadata ]; then
                  if grep -q 'dep = "1"' Cargo.toml; then
                    cp Cargo.lock.unwidened Cargo.lock
                    cat metadata.unwidened.json
                    exit 0
                  fi
                  cp Cargo.lock.forked Cargo.lock
                {widened_probe}fi
                echo 'error: failed to select a version for the requirement `dep = "^1"`' >&2
                echo 'required by package `holder v1.0.0`' >&2
                exit 1
            "#},
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let cache = tempfile::tempdir()?;
        let mut tool = CargoTool::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        tool.cargo = Cargo::with_bin(script.as_str());
        let project = Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
            exclude_newer_package: std::collections::BTreeMap::new(),
            generated_members: cooldown_core::GeneratedMembers::undeclared(),
        };
        Ok((project, tool))
    }

    #[cfg(unix)]
    fn member_widen_change() -> Change {
        let mut planned = change("dep", "1.0.0", "2.0.0", false);
        planned.members = vec![cooldown_core::MemberRef {
            name: "app".to_string(),
            path: ".".to_string(),
        }];
        planned
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tentative_member_widen_rollback_restores_the_probe_forked_lock() -> eyre::Result<()> {
        // The member-candidate rollback defect: the precise pin is rejected, but the *unlocked*
        // metadata probe between pin and rollback re-locks the widened requirement, forking dep
        // at 2.1.0 — newer than the 2.0.0 target, so the member-aware reach check fails. The
        // rollback must restore the staged lock beside the manifests: restoring the manifests
        // alone would keep a fork no surviving requirement admits, visible to `resolver_lock` and
        // the preserve preflight until a later unlocked resolve incidentally healed it.
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = member_widen_fixture(root, false)?;
        let plan = Plan {
            changes: vec![member_widen_change()],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        tool.whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.toml"))?,
            MEMBER_WIDEN_MANIFEST,
            "the unlandable widen is restored byte-identically"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            MEMBER_WIDEN_LOCK,
            "the staged lock the probe forked is restored with the manifests, \
             not left for a later resolve to heal"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tentative_member_widen_rollback_restores_the_lock_when_the_probe_errs()
    -> eyre::Result<()> {
        // The sibling rollback branch: the widened-manifest probe forks the lock *and then* fails
        // as a resolver rejection. That branch must likewise restore the staged lock with the
        // manifests, and the candidate keeps a rejection detail for its skip row.
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = member_widen_fixture(root, true)?;
        let plan = Plan {
            changes: vec![member_widen_change()],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        let rejections = tool
            .whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.toml"))?,
            MEMBER_WIDEN_MANIFEST,
            "the unlandable widen is restored byte-identically"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("Cargo.lock"))?,
            MEMBER_WIDEN_LOCK,
            "the probe-error rollback restores the staged lock the probe forked"
        );
        assert!(
            rejections.contains_key(&("dep".to_string(), "1.0.0".to_string(), "2.0.0".to_string())),
            "the held candidate keeps its rejection detail"
        );
        Ok(())
    }
}
