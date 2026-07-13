//! The Rust/Cargo [`Tool`]: detection, the resolved graph via `cargo metadata`, classified
//! releases from the crates.io sparse index, and `cargo`-driven apply/build.
//!
//! Cargo has no publish-date cutoff flag (no `--exclude-newer` equivalent), so the cooldown window
//! is realized entirely in [`cooldown_core`]: the crates.io sparse index supplies publish times, the
//! core computes each crate's newest-within-window target, and this adapter applies those as concrete
//! `cargo update --precise <version>` pins. Apply re-resolves the **whole** graph by issuing all of
//! a project's planned pins (one `cargo update -p <spec> --precise V` per pin — cargo silently drops
//! all but the first spec when several share one `--precise`) as one logical unit, then builds the
//! report from the FULL before/after `Cargo.lock` diff — not from per-change outcomes. So every net
//! version change is surfaced (the planned moves, the collateral moves the re-resolve forces on
//! non-candidate crates, and the candidates a mutually-exclusive `=`-pin or single-major shared
//! transitive leaves held), and a converged graph re-applies to a byte-stable fixed point.

use crate::cargocmd::{CRATES_IO_SOURCE, Cargo, ResolvedGraph};
use crate::index::{CRATES_IO, CratesIoIndex};
use crate::manifest;
use crate::native::parse_native;
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{build_registry_releases, verify_current_report};
use cooldown_core::{
    ApplyReport, Capabilities, Change, DepScope, Dependency, FetchContext, LockVerifyReport,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality, ResolveInputs,
    Result, RewriteMode, SkipReason, Skipped, ToolId, ToolRead, ToolWrite, UpdateKind,
    VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use std::collections::{BTreeMap, BTreeSet};

/// The [`ToolId`] identifying the Rust/Cargo tool (`"cargo"`).
pub const CARGO_ID: ToolId = ToolId("cargo");

/// The Rust/Cargo implementation of the [`Tool`] port.
///
/// Pairs the crates.io sparse-index client ([`CratesIoIndex`]) with a [`Cargo`]
/// CLI wrapper: the index supplies publish times and the release set, while
/// `cargo` resolves the dependency graph and applies precise version changes.
pub struct CargoTool {
    index: CratesIoIndex,
    cargo: Cargo,
}

impl CargoTool {
    /// Creates an tool from an existing crates.io [`CratesIoIndex`] client.
    ///
    /// The [`Cargo`] CLI wrapper is constructed with its defaults (honoring the
    /// `COOLDOWN_CARGO` environment override).
    #[must_use]
    pub fn new(index: CratesIoIndex) -> Self {
        CargoTool {
            index,
            cargo: Cargo::new(),
        }
    }

    /// Creates an tool backed by the shared HTTP layer, building the index for you.
    ///
    /// Convenience constructor equivalent to `CargoTool::new(CratesIoIndex::new(http))`.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        CargoTool::new(CratesIoIndex::new(http))
    }
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
/// the currently pinned version, used to compute each release's [`UpdateKind`](cooldown_core::UpdateKind)
/// via [`version::classify_kind`].
#[must_use]
pub fn build_releases(current: &str, raw: Vec<cooldown_core::RawRelease>) -> Vec<Release> {
    build_registry_releases(
        current,
        raw,
        |value| version::parse(value).is_some(),
        version::compare,
        version::major_key,
        version::classify_kind,
        classify_quality,
    )
}

#[async_trait]
impl ToolRead for CargoTool {
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
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        // A `Cargo.lock` marks a workspace root: `cargo metadata` there already covers every
        // member, so nested lockfiles below it are not separate projects.
        ProjectMarker {
            lockfile: "Cargo.lock",
            manifest: "Cargo.toml",
            alternate_manifests: &[],
            workspace_root: true,
        }
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let graph = self.cargo.metadata(&project.root).await?;
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
                current: Version::new(info.version.clone()),
                current_quality: classify_quality(&info.version),
                direct,
                artifacts: Vec::new(),
                graph_floor,
                graph_ceiling: (!pinned && graph.is_graph_capped(&info.name, &info.version))
                    .then(|| Version::new(info.version.clone())),
                // Direct deps are attributed to their declarers; a transitive dep is attributed to
                // the members that reach it through the graph (rendered as "via …").
                members: if direct {
                    graph.direct_members(id)
                } else {
                    graph.reaching_members(id)
                },
                pinned,
            });
        }
        Ok(deps)
    }

    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>> {
        parse_native(&project.manifest)
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<LockVerifyReport> {
        match self.cargo.verify_locked(&project.root).await {
            Ok(ok) => Ok(verify_current_report(
                ok,
                "Cargo.lock is current",
                "Cargo.lock is stale; run `cargo update` or `cargo generate-lockfile`",
            )),
            Err(e) => Err(e),
        }
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

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self
            .index
            .published_at(&dep.package, &dep.current, &[])
            .await?;
        Ok(Release {
            version: dep.current.clone(),
            order: ReleaseOrder(Vec::new()),
            major: version::major_key(dep.current.as_str()),
            kind_from_current: None,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

/// A `(name, major)` slot key. Cargo coexists multiple majors of one crate (`serde 0.9` and
/// `serde 1.0` can both be in the lock), so a name alone is ambiguous; the slot a `--precise` pin
/// moves and the slot the before/after diff compares is the `(name, major)` pair. A net version
/// change within one slot is a *move*; a slot that appears or disappears is graph-shape churn the
/// diff ignores (a consequence of a reported move, not a silent version change).
type SlotKey = (String, String);

/// Every registry version present in each Cargo compatibility slot.
type LockedSlots = BTreeMap<SlotKey, BTreeSet<String>>;

fn locked_slots(lock: &CargoLock) -> LockedSlots {
    matching_locked_slots(lock, LockPackage::is_registry)
}

fn crates_io_locked_slots(lock: &CargoLock) -> LockedSlots {
    matching_locked_slots(lock, LockPackage::is_crates_io)
}

fn matching_locked_slots(lock: &CargoLock, include: impl Fn(&LockPackage) -> bool) -> LockedSlots {
    let mut slots: LockedSlots = BTreeMap::new();
    for package in &lock.package {
        let (Some(version), true) = (package.version.as_deref(), include(package)) else {
            continue;
        };
        slots
            .entry((package.name.clone(), version::major_key(version).0))
            .or_default()
            .insert(version.to_string());
    }
    slots
}

/// The resolved registry-crate versions of a `Cargo.lock`, keyed per `(name, major)` slot — the
/// snapshot `apply` diffs before/after the whole-graph re-resolve to report *every* net version
/// change (the planned moves, the collateral moves the resolve forced for consistency, and the
/// candidates a conflict left held). Path/git/workspace packages carry no comparable registry
/// version and are skipped. When two nodes share a `(name, major)` slot (rare; only via distinct
/// `source` registries), the highest version wins so the slot is single-valued.
fn locked_versions(lock: &CargoLock) -> BTreeMap<SlotKey, String> {
    highest_locked_versions(locked_slots(lock))
}

fn crates_io_locked_versions(lock: &CargoLock) -> BTreeMap<SlotKey, String> {
    highest_locked_versions(crates_io_locked_slots(lock))
}

fn highest_locked_versions(slots: LockedSlots) -> BTreeMap<SlotKey, String> {
    slots
        .into_iter()
        .filter_map(|(key, versions)| {
            versions
                .into_iter()
                .max_by(|left, right| version::compare(left, right))
                .map(|version| (key, version))
        })
        .collect()
}

/// The `Cargo.lock`'s `[[package]]` array, parsed for the before/after version diff. Only the
/// fields the diff needs are read; `cargo` owns the canonical format.
#[derive(serde::Deserialize)]
struct CargoLock {
    #[serde(default)]
    package: Vec<LockPackage>,
}

#[derive(serde::Deserialize)]
struct LockPackage {
    name: String,
    #[serde(default)]
    version: Option<String>,
    /// The source URL. Absent for path/workspace members; present for registry and git crates. Only
    /// registry crates have a comparable, fetchable version, so the diff keeps only those.
    #[serde(default)]
    source: Option<String>,
}

impl LockPackage {
    /// Whether this locked package came from a registry (crates.io or an alternate registry), the
    /// only source kind whose version the cooldown diff can move and compare. Git and path/workspace
    /// sources are excluded.
    fn is_registry(&self) -> bool {
        self.source
            .as_deref()
            .is_some_and(|source| source.starts_with("registry+"))
    }

    fn is_crates_io(&self) -> bool {
        self.source.as_deref() == Some(CRATES_IO_SOURCE)
    }
}

impl CargoLock {
    fn parse(content: &str) -> Result<Self> {
        toml::from_str(content)
            .map_err(|err| cooldown_core::CoreError::LockUnreadable(format!("Cargo.lock: {err}")))
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
    let slots = crates_io_locked_slots(lock);
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

/// The net version changes of the before/after lock diff that `applied` does not already report,
/// as sorted collateral rows.
///
/// Exclusion is by exact `(name, from, to)` move, not by planned package name: a planned candidate
/// the resolve *held* can still have been floated off its baseline by a sibling pin, and that real
/// movement must surface beside its held skip row instead of being silently dropped.
fn collateral_changes(
    before: &BTreeMap<SlotKey, String>,
    after: &BTreeMap<SlotKey, String>,
    applied: &[Change],
) -> Vec<Change> {
    let reported: BTreeSet<(&str, &str, &str)> = applied
        .iter()
        .map(|change| {
            (
                change.package.name.as_str(),
                change.from.as_str(),
                change.to.as_str(),
            )
        })
        .collect();
    let mut changes: Vec<Change> = before
        .iter()
        .filter_map(|((name, _), from)| {
            let to = after.get(&(name.clone(), version::major_key(from).0))?;
            (version::compare(from, to).is_ne()
                && !reported.contains(&(name.as_str(), from.as_str(), to.as_str())))
            .then(|| collateral_change(name, from, to))
        })
        .collect();
    changes.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    changes
}

/// A net version change `apply` derived from the before/after lock diff that no planned row
/// reports — collateral movement the whole-graph re-resolve forced. Reported so no crate's
/// version change is ever silent: a transitive pushed backward to keep the lock consistent, or
/// matured down by `fix`, surfaces as its own report row.
fn collateral_change(name: &str, from: &str, to: &str) -> Change {
    let downgrade = version::compare(to, from).is_lt();
    Change {
        package: PackageId::new(CARGO_ID, name.to_string(), Some(CRATES_IO.to_string())),
        from: Version::new(from.to_string()),
        to: Version::new(to.to_string()),
        // A collateral move is transitive consistency churn, not a directly-declared bump; its kind
        // is informational only and `Minor` is the neutral label the renderer shows.
        kind: cooldown_core::UpdateKind::Minor,
        downgrade,
        direct: false,
        members: Vec::new(),
    }
}

/// The crate whose `=x.y.z` requirement structurally holds `held` out of the graph at `target` —
/// the cargo analog of uv's `blocking_requirer`. A held cargo candidate is almost always blocked by
/// an exact pin on a shared single-major node (cargo coexists distinct majors, so an open caret
/// range rarely conflicts): some *other* crate's `graph_ceiling` (an active `=` edge) caps the
/// shared node below the candidate's target. Returns the requirer that caps `held`, or `None` so the
/// caller falls back to the generic "the resolver rejected this change".
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

impl CargoTool {
    /// Re-resolve the **whole** graph once under cooldown's window, then build the report from the
    /// full before/after `Cargo.lock` diff. `upgrade` is informational for the rewrite policy; cargo
    /// has no date cutoff, so every move is expressed as a concrete `--precise` pin computed by the
    /// core. Widening for `Always`/`Auto` happens before pinning so a cross-major target is admitted.
    async fn whole_graph_resolve(&self, project: &Project, plan: &Plan) -> Result<()> {
        // Widen the owning manifest constraints for all candidates up front under `Always`; under
        // `Auto`, widen only those whose own declared requirement would otherwise cap them below the
        // target (a cross-major bump). The pin itself follows.
        if matches!(plan.rewrite, RewriteMode::Always) {
            for change in &plan.changes {
                manifest::widen_constraint(
                    &project.root,
                    &change.members,
                    &change.package.name,
                    change.to.as_str(),
                )?;
            }
        }
        self.pin_batch(project, &plan.changes).await?;

        if matches!(plan.rewrite, RewriteMode::Auto) {
            // Widen only the candidates the pin batch could not place at their target because their
            // own declared requirement caps them, then re-pin. A candidate still short after a no-op
            // widen round is blocked by another crate (a real conflict the diff reports), so the loop
            // stops widening.
            // The member-aware reach check is the only thing in this loop that needs the resolved
            // graph, so skip the `cargo metadata` spawn entirely when no candidate is a direct
            // member dep. When it is needed, fail closed: falling back to the lock-slot check is the
            // false positive this loop exists to avoid.
            let needs_graph = plan.changes.iter().any(needs_member_graph);
            for _ in 0..plan.changes.len() {
                let after = crates_io_locked_versions(&read_lock(project)?);
                let graph = if needs_graph {
                    Some(self.cargo.metadata(&project.root).await?)
                } else {
                    None
                };
                let mut widened_any = false;
                let mut short = Vec::new();
                for change in &plan.changes {
                    if reached_after(&after, graph.as_ref(), change) {
                        continue;
                    }
                    short.push(change.clone());
                    if !manifest::widen_constraint(
                        &project.root,
                        &change.members,
                        &change.package.name,
                        change.to.as_str(),
                    )?
                    .modified
                    .is_empty()
                    {
                        widened_any = true;
                    }
                }
                if !widened_any {
                    break;
                }
                self.pin_batch(project, &short).await?;
            }
        }
        Ok(())
    }

    /// Applies all `changes` as one logical unit, driving each to its exact target.
    ///
    /// Cargo accepts several `-p` specs beside one `--precise` but silently applies only the first
    /// spec, so every planned pin needs its own command. Those commands are still graph-wide: one
    /// pin can move another planned node, or remove the lock entry a later pin would have
    /// addressed. Each pass therefore re-reads the lock before every pin and pins the node
    /// [`current_selector`] picks (the planned `from` line while it exists, the unique off-target
    /// node once another pin moved it); passes repeat until one makes no progress or the lock
    /// revisits an earlier state. Resolver rejections remain non-fatal held candidates the final
    /// diff reports; local environment failures abort.
    async fn pin_batch(&self, project: &Project, changes: &[Change]) -> Result<()> {
        // Direct workspace members can emit sibling changes sharing `(package, from, to)`; those
        // are one lock move, so issue each distinct spec once, in a deterministic order.
        let mut worklist: Vec<&Change> = changes.iter().collect();
        worklist.sort_by(|a, b| {
            a.package
                .name
                .cmp(&b.package.name)
                .then_with(|| a.package.registry.cmp(&b.package.registry))
                .then_with(|| a.from.as_str().cmp(b.from.as_str()))
                .then_with(|| a.to.as_str().cmp(b.to.as_str()))
        });
        worklist.dedup_by(|a, b| a.package == b.package && a.from == b.from && a.to == b.to);

        let mut seen = BTreeSet::new();
        for _ in 0..worklist.len().saturating_add(1) {
            let before = locked_slots(&read_lock(project)?);
            if !seen.insert(before.clone()) {
                break;
            }
            let mut attempted = false;
            for change in &worklist {
                let lock = read_lock(project)?;
                let Some(current) = current_selector(&lock, change) else {
                    continue;
                };
                attempted = true;
                self.update_precise(project, &change.package.name, &current, change.to.as_str())
                    .await?;
            }
            let after = locked_slots(&read_lock(project)?);
            if !attempted || after == before {
                break;
            }
        }
        Ok(())
    }

    /// Issues one tolerant precise pin, separating resolver rejection from local breakage.
    ///
    /// A rejected precise pin is a resolver outcome, so the final lock diff reports the candidate
    /// held. Broken local state must propagate; otherwise disk-full or spawn failures would
    /// masquerade as a conflict in the candidate set.
    async fn update_precise(
        &self,
        project: &Project,
        name: &str,
        from: &str,
        to: &str,
    ) -> Result<()> {
        if let Err(err) = self
            .cargo
            .update_precise_crates_io(&project.root, name, from, to)
            .await
            && err.is_local_environment_failure()
        {
            return Err(err);
        }
        Ok(())
    }
}

fn read_lock(project: &Project) -> Result<CargoLock> {
    let content = std::fs::read_to_string(project.root.join("Cargo.lock"))?;
    CargoLock::parse(&content)
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
    fn resolve_inputs(&self) -> ResolveInputs {
        // `cargo update`/`generate-lockfile` validates every workspace member's declared targets, so
        // the throwaway copy must include `.rs` source — a member with an empty `src/` errors with "no
        // targets specified". Source is small (the bulk of a repo is build `target/`, which is pruned).
        ResolveInputs {
            source_extensions: &["rs"],
            ..ResolveInputs::DEFAULT
        }
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
        let mut files = Vec::with_capacity(relative.len());
        for rel in relative {
            files.push(ProjectMutationJournal::capture_file(&project.root, &rel)?);
        }
        Ok(ProjectMutationJournal { files })
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }

        // The pre-apply lock, taken from the journal (`mutation_journal` captured `Cargo.lock` before
        // the re-resolve). The batched precise pins emit one consistent lock; the report is the diff
        // of this snapshot against the result, so *every* net version change is surfaced. A
        // missing/unparsable snapshot leaves `before` empty, so a crate that moved is still reported
        // (never silent).
        let before = journal
            .files
            .iter()
            .find(|file| file.path == Utf8Path::new("Cargo.lock"))
            .and_then(|file| file.contents.as_deref())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|content| CargoLock::parse(content).ok())
            .map(|lock| locked_versions(&lock))
            .unwrap_or_default();

        // The whole graph is re-resolved as one logical batch: each concrete pin gets its own Cargo
        // invocation, repeated to a bounded fixed point because one invocation may move a package
        // another planned pin still needs to address. The diff below surfaces every net move.
        match self.whole_graph_resolve(project, plan).await {
            Ok(()) => {}
            Err(err) if err.is_tool_spawn_failure() => return Err(err),
            // The joint resolve is unsatisfiable as a whole (a `=`-pin conflict or an unfetchable
            // version). Propagate so the caller's `apply_resilient` can isolate the offending
            // candidate(s) and apply the rest, instead of holding every candidate. Local environment
            // failures propagate through `apply_resilient` without bisection. The caller restores the
            // journal, so no partial lock is kept.
            Err(err) => return Err(err),
        }

        let after_lock = read_lock(project)?;
        let after = locked_versions(&after_lock);
        let crates_io_after = crates_io_locked_versions(&after_lock);
        // The resolved graph proves direct member changes reached the target and names the crate
        // whose `=`-pin holds a short candidate back.
        let needs_graph = plan.changes.iter().any(needs_member_graph);
        let graph = if needs_graph {
            Some(self.cargo.metadata(&project.root).await?)
        } else {
            self.cargo.metadata(&project.root).await.ok()
        };
        // Each planned candidate either reached cooldown's target (its newest-within-window) —
        // reported applied — or fell short because a mutually-exclusive `=`-pin or single-major shared
        // transitive won — reported held, naming the blocker.
        for change in &plan.changes {
            if reached_after(&crates_io_after, graph.as_ref(), change) {
                report.applied.push(change.clone());
            } else {
                let offender = graph
                    .as_ref()
                    .and_then(|graph| {
                        blocking_requirer(graph, &change.package.name, change.to.as_str())
                    })
                    .unwrap_or_else(|| change.package.name.clone());
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::ResolverConflict,
                    offending: Some(PackageId::new(
                        CARGO_ID,
                        offender,
                        Some(CRATES_IO.to_string()),
                    )),
                });
            }
        }

        // The hard requirement: no net version change to *any* crate may be omitted. Every moved
        // slot the applied rows above do not already report is surfaced as its own collateral
        // applied row: a transitive pushed backward for consistency, a crate matured down by `fix`,
        // or a *held* candidate the resolve still floated off its baseline (whose skip row alone
        // would hide that real move).
        let collateral = collateral_changes(&before, &after, &report.applied);
        report.applied.extend(collateral);
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cargo.build(&project.root).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use cooldown_adapter_util::skipped_on_apply_error;
    use cooldown_core::CoreError;
    use indoc::indoc;

    fn lock_with(packages: &[(&str, &str)]) -> CargoLock {
        use std::fmt::Write as _;
        let mut content = String::from("version = 4\n");
        for (name, version) in packages {
            write!(
                content,
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n"
            )
            .expect("writing to a String never fails");
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

    #[test]
    fn locked_versions_skips_non_registry_and_keys_per_major() {
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
        let slots = locked_versions(&lock);
        // The path/workspace member `demo` and the git source are excluded; the two serde majors are
        // distinct slots, and semantic ordering chooses 1.0.197 over lexically-greater 1.0.99.
        assert_eq!(
            slots.get(&("serde".into(), "1".into())).map(String::as_str),
            Some("1.0.197")
        );
        assert_eq!(
            slots
                .get(&("serde".into(), "0.9".into()))
                .map(String::as_str),
            Some("0.9.15")
        );
        assert!(!slots.keys().any(|(name, _)| name == "demo"));
        assert!(!slots.keys().any(|(name, _)| name == "local-git"));
    }

    #[test]
    fn reached_requires_the_exact_target_in_its_major_slot() {
        let after = locked_versions(&lock_with(&[("serde", "1.0.200"), ("syn", "2.0.50")]));
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
                &crates_io_locked_versions(&private_target),
                &change("serde", "1.0.100", "1.0.200", false),
            ),
            "an alternate-registry target does not satisfy a crates.io plan"
        );
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
        let after = locked_versions(&lock_with(&[("nix", "0.28.0"), ("nix", "0.31.3")]));
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

        let rewrite =
            manifest::widen_constraint(&root, std::slice::from_ref(&mcp_member), "nix", "0.31.3")
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

    #[test]
    fn collateral_change_surfaces_a_forced_non_candidate_downgrade() {
        // Raising `a` forces the shared transitive `shared` from 1.1.0 down to 1.0.0 as a consistency
        // move. No applied row reports `shared`, so the diff must surface it as its own collateral
        // row — the silent drift the earlier per-precise-pin design allowed.
        let before = locked_versions(&lock_with(&[("a", "1.0.0"), ("shared", "1.1.0")]));
        let after = locked_versions(&lock_with(&[("a", "2.0.0"), ("shared", "1.0.0")]));
        let applied = [change("a", "1.0.0", "2.0.0", false)];
        let collateral = collateral_changes(&before, &after, &applied);
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
    fn collateral_change_excludes_applied_and_unchanged_packages() {
        // `a`'s move is already told by its applied row (no duplicate), `b` is unchanged (no row),
        // `c` is an unplanned forward move (a real collateral change). Only `c` is surfaced.
        let before = locked_versions(&lock_with(&[
            ("a", "2.0.0"),
            ("b", "2.0.0"),
            ("c", "1.0.0"),
        ]));
        let after = locked_versions(&lock_with(&[
            ("a", "1.0.0"),
            ("b", "2.0.0"),
            ("c", "1.5.0"),
        ]));
        let applied = [change("a", "2.0.0", "1.0.0", true)];
        let collateral = collateral_changes(&before, &after, &applied);
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "c");
        assert!(!collateral[0].downgrade);
    }

    #[test]
    fn collateral_changes_surface_a_held_candidates_real_movement() {
        // A held planned candidate has no applied row, yet a sibling pin still floated it off its
        // baseline. That net move must surface as a collateral row beside the held skip instead of
        // being silently dropped behind the planned name.
        let before = locked_versions(&lock_with(&[("referencing", "0.46.5")]));
        let after = locked_versions(&lock_with(&[("referencing", "0.46.10")]));
        let collateral = collateral_changes(&before, &after, &[]);
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "referencing");
        assert_eq!(collateral[0].from.as_str(), "0.46.5");
        assert_eq!(collateral[0].to.as_str(), "0.46.10");

        // Once the candidate reaches its target, its applied row already tells the move: no
        // duplicate collateral row.
        let landed = locked_versions(&lock_with(&[("referencing", "0.46.10")]));
        let applied = [change("referencing", "0.46.5", "0.46.10", false)];
        assert!(collateral_changes(&before, &landed, &applied).is_empty());
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
        assert!(matches!(result, Err(CoreError::ToolSpawn { .. })));
    }

    #[tokio::test]
    async fn mutation_journal_restore_removes_lock_created_after_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("write manifest");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let eco = CargoTool::from_http(
            cooldown_registry::SharedHttp::new(
                cache_dir.path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        let project = Project {
            root: root.clone(),
            kind: CARGO_ID,
            manifest,
            exclude_newer: None,
        };

        let journal = eco
            .mutation_journal(&project, &Plan::default())
            .await
            .expect("journal");
        let lock = root.join("Cargo.lock");
        std::fs::write(&lock, "generated").expect("write lock");

        journal.restore(&project.root).expect("restore");
        assert!(!lock.exists());
    }
}
