//! The generic JavaScript/TypeScript [`Tool`]: detection, the resolved graph from a lockfile, npm
//! registry publish times, and driver-backed re-resolution/apply. The lockfile format and driver
//! binary are supplied by a [`NodeLock`] type parameter, so npm, pnpm, yarn, and bun are all the
//! same adapter specialised over their lock format — they share the npm registry and version model
//! and differ only in how their lock is parsed and how their CLI re-pins a dependency.

use crate::lock::{NameVersion, NodeLock};
use crate::manifest;
use crate::nodecmd::NodeCmd;
use crate::registry::{NPM, NpmRegistry};
use crate::version::{self, RangeMatch};
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
    WindowSpec,
};
use cooldown_registry::SharedHttp;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::marker::PhantomData;

struct WholeGraphInputs {
    exact_pins: Vec<(String, String)>,
    importer_filters: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum ConfigStringList {
    One(String),
    Many(Vec<String>),
}

impl ConfigStringList {
    fn into_vec(self) -> Vec<String> {
        match self {
            ConfigStringList::One(value) => vec![value],
            ConfigStringList::Many(values) => values,
        }
    }
}

impl Default for ConfigStringList {
    fn default() -> Self {
        ConfigStringList::Many(Vec::new())
    }
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
    Ok(ProjectMutationJournal { files })
}

/// The result and exact write-set state observed when an adapter-owned command returned.
struct OwnedStep {
    result: Result<()>,
    postimage: cooldown_core::ProjectMutationState,
}

impl OwnedStep {
    fn capture(
        result: Result<()>,
        journal: &ProjectMutationJournal,
        root: &Utf8Path,
    ) -> Result<Self> {
        Ok(OwnedStep {
            result,
            postimage: journal.capture_state(root)?,
        })
    }
}

#[async_trait]
trait CandidateCommand {
    async fn run_candidate(&self, root: &Utf8Path, args: &[String]) -> Result<()>;
}

#[async_trait]
impl CandidateCommand for NodeCmd {
    async fn run_candidate(&self, root: &Utf8Path, args: &[String]) -> Result<()> {
        self.run(root, args).await
    }
}

/// Restores a snapshot after an adapter-owned step while refusing subsequent filesystem drift.
fn restore_after_owned_step(
    journal: &ProjectMutationJournal,
    root: &Utf8Path,
    postimage: &cooldown_core::ProjectMutationState,
) -> Result<()> {
    journal.restore_if_unchanged(root, postimage)
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

/// Chooses the lock-only driver command for one change, when the package manager supports one.
///
/// In `Auto` mode, when the package manager offers a lock-only update (only pnpm does) and the target
/// already satisfies the declared `package.json` range, move just the lock and leave the range as the
/// author wrote it. Otherwise the caller applies only the manifest edits authorized by the rewrite
/// mode, then lands the exact target where the manager supports it. The in-range check happens up
/// front because lock-only commands re-pin whatever version they are given without validating it,
/// so an out-of-range version would leave the lock inconsistent with `package.json`.
fn lockonly_command<L: NodeLock>(
    project: &Project,
    change: &Change,
    mode: RewriteMode,
) -> Result<Option<Vec<String>>> {
    let name = &change.package.name;
    let version = change.to.as_str();
    if mode == RewriteMode::Auto
        && let Some(lockonly) = L::lockonly_update_args(name, version)
        && target_in_declared_range(project, change)?
    {
        return Ok(Some(lockonly));
    }
    Ok(None)
}

/// The transaction that lands one candidate after cooldown has applied its authorized manifest
/// edits and captured those resulting bytes.
enum CandidateLanding {
    /// The command preserves manifests itself; the snapshot re-establishes the authorized state if
    /// a cutoff fallback first restores the candidate baseline.
    Direct {
        command: Vec<String>,
        authorized_manifests: ProjectMutationJournal,
    },
    /// The exact pin may save a different manifest range, so the authorized bytes are restored and
    /// the lock is resynchronized before the result is judged.
    PinRestoreResync {
        pin: Vec<String>,
        authorized_manifests: ProjectMutationJournal,
        resync: Vec<String>,
    },
}

impl CandidateLanding {
    fn command(&self) -> &[String] {
        match self {
            CandidateLanding::Direct { command, .. } => command,
            CandidateLanding::PinRestoreResync { pin, .. } => pin,
        }
    }

    fn authorized_manifests(&self) -> &ProjectMutationJournal {
        match self {
            CandidateLanding::Direct {
                authorized_manifests,
                ..
            }
            | CandidateLanding::PinRestoreResync {
                authorized_manifests,
                ..
            } => authorized_manifests,
        }
    }
}

/// Plans how to land `change`, widening the declaring manifests when that is what this manager and
/// mode require. `None` means no manifest declares the dependency, so the caller reports it not
/// eligible rather than risk adding a spurious root dependency.
///
/// Eligibility is a question about the *declarations*, not about what the widen happened to write: a
/// dependency declared only in a published-contract field has an empty write set by design (nothing
/// may rewrite it), and reading that as "undeclared" would skip a move whose lock can still be
/// landed exactly.
fn candidate_landing<L: NodeLock>(
    project: &Project,
    change: &Change,
    mode: RewriteMode,
) -> Result<Option<CandidateLanding>> {
    if let Some(args) = lockonly_command::<L>(project, change, mode)? {
        return Ok(Some(CandidateLanding::Direct {
            command: args,
            authorized_manifests: manifest_snapshot(project, change)?,
        }));
    }
    let declarations =
        manifest::declarations(&project.root, &change.members, &change.package.name)?;
    if declarations.absent() {
        return Ok(None);
    }
    let preserving_pin = preserving_pin::<L>(project, change, declarations.install_workspaces());
    // Without an exact preserving pin, shifting even an in-range declaration is the only way to
    // steer a bare relock to the planned target.
    let manifest_mode = if mode == RewriteMode::Auto && preserving_pin.is_none() {
        RewriteMode::Always
    } else {
        mode
    };
    let rewrite = manifest::widen_constraints(
        &project.root,
        &change.members,
        &change.package.name,
        change.to.as_str(),
        manifest_mode,
    )?;
    if mode == RewriteMode::Auto
        && rewrite.modified.is_empty()
        && declarations.has_install()
        && matches!(
            &preserving_pin,
            Some(crate::lock::PreservingPin::PinRestoreResync { .. })
        )
    {
        // npm's resync can select the old lock again when every install range remains compatible.
        // Shift those ranges only when no declaration needed widening; if one did, its edit already
        // steers the resync and every compatible sibling remains untouched.
        manifest::widen_constraints(
            &project.root,
            &change.members,
            &change.package.name,
            change.to.as_str(),
            RewriteMode::Always,
        )?;
    }
    let authorized_manifests = manifest_snapshot(project, change)?;
    let landing = match preserving_pin {
        Some(crate::lock::PreservingPin::Direct(command)) => CandidateLanding::Direct {
            command,
            authorized_manifests,
        },
        Some(crate::lock::PreservingPin::PinRestoreResync { pin, resync }) => {
            CandidateLanding::PinRestoreResync {
                pin,
                authorized_manifests,
                resync,
            }
        }
        None => {
            let before = absolute_cutoff_from_project(
                project.exclude_newer.as_deref(),
                jiff::Timestamp::now(),
            );
            CandidateLanding::Direct {
                command: L::relock_args(before.as_deref()),
                authorized_manifests,
            }
        }
    };
    Ok(Some(landing))
}

/// The manager's exact pin for this change, including any restore/resync transaction needed to keep
/// the authorized manifest bytes authoritative.
fn preserving_pin<L: NodeLock>(
    project: &Project,
    change: &Change,
    workspaces: &[String],
) -> Option<crate::lock::PreservingPin> {
    let before =
        absolute_cutoff_from_project(project.exclude_newer.as_deref(), jiff::Timestamp::now());
    L::preserving_pin(
        &change.package.name,
        change.to.as_str(),
        before.as_deref(),
        workspaces,
    )
}

/// Captures just the `package.json` files a change could touch, leaving the lockfile outside the
/// snapshot so the landing transaction can restore the authorized manifests and resynchronize the
/// new pin.
fn manifest_snapshot(project: &Project, change: &Change) -> Result<ProjectMutationJournal> {
    let mut files = Vec::new();
    for rel in manifest::manifest_rels(&change.members) {
        files.push(ProjectMutationJournal::capture_file(&project.root, &rel)?);
    }
    Ok(ProjectMutationJournal { files })
}

/// Whether the change's target satisfies every range declared for it in the manifests that could own
/// it (the project root, plus each declaring member). A dependency not found in any of them returns
/// `false`, so the caller rewrites rather than risk an inconsistent lock.
fn target_in_declared_range(project: &Project, change: &Change) -> Result<bool> {
    let mut found = false;
    for manifest in candidate_manifests(project, change) {
        if let Some(range) = manifest::declared_range(&manifest, &change.package.name)? {
            found = true;
            if !version::version_in_range(&range, change.to.as_str()) {
                return Ok(false);
            }
        }
    }
    Ok(found)
}

/// The `package.json` manifests that might declare a change's dependency: the project root plus each
/// declaring workspace member, root-relative paths resolved against the project root.
fn candidate_manifests(project: &Project, change: &Change) -> Vec<Utf8PathBuf> {
    manifest::manifest_rels(&change.members)
        .into_iter()
        .map(|rel| project.root.join(rel))
        .collect()
}

/// cooldown's resolution window as pnpm's rolling `minimumReleaseAge` minute count, derived from the
/// project's `exclude_newer` cutoff (the same value uv hands its resolver as `--exclude-newer`).
///
/// pnpm has no absolute publish-date cutoff, only a rolling "exclude releases younger than N minutes"
/// — but the two coincide: excluding everything younger than `now - cutoff` is exactly excluding
/// everything published after `cutoff`. So both forms the application emits map to a minute count:
/// a *relative* span (`"14 days"`, `"36 hours"`, `"90 seconds"`) for an age window converts directly,
/// and an absolute RFC3339 instant (a `--freeze` cutoff, or the `now` instant a `Latest`/opt-out
/// passes) converts as `now - instant`. `now` is supplied by the caller so the conversion is
/// deterministic under a fixed clock. A future instant or a zero/negative span yields `None`
/// (nothing to exclude).
fn window_minutes_from_cutoff(cutoff: Option<&str>, now: jiff::Timestamp) -> Option<i64> {
    let cutoff = cutoff?.trim();
    if let Some((count, unit)) = cutoff.split_once(' ')
        && let Ok(count) = count.parse::<i64>()
    {
        let minutes = match unit.trim_end_matches('s') {
            "day" => count.checked_mul(24 * 60)?,
            "hour" => count.checked_mul(60)?,
            "minute" => count,
            // A second-granularity window rounds up to a whole minute so a sub-minute age still
            // excludes the just-published release rather than silently disabling the cooldown.
            "second" => count.checked_add(59)? / 60,
            _ => return None,
        };
        return (minutes > 0).then_some(minutes);
    }
    // An absolute instant (freeze / `now` opt-out): the rolling age that reproduces it is `now - it`.
    let instant: jiff::Timestamp = cutoff.parse().ok()?;
    let minutes = now.duration_since(instant).as_secs() / 60;
    (minutes > 0).then_some(minutes)
}

/// Converts the application's stable project cutoff into the absolute instant npm's `--before`
/// option requires.
///
/// Age windows are stored as relative spans so they remain stable between runs. npm delegates
/// `--before` to JavaScript date parsing, which does not understand those spans, so each command
/// realizes the span against its current instant. Freeze and latest cutoffs are already absolute.
fn absolute_cutoff_from_project(cutoff: Option<&str>, now: jiff::Timestamp) -> Option<String> {
    let cutoff = cutoff?.trim();
    if let Ok(instant) = cutoff.parse::<jiff::Timestamp>() {
        return Some(instant.to_string());
    }
    let duration = cooldown_core::duration::parse_duration(cutoff).ok()?;
    now.checked_sub(duration)
        .ok()
        .map(|instant| instant.to_string())
}

/// The same command with its `--before=` cutoff removed, or `None` when no cutoff was present —
/// the retry the caller may attempt when the historical-tree resolve is unsatisfiable.
fn without_before(args: &[String]) -> Option<Vec<String>> {
    let filtered: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with("--before="))
        .cloned()
        .collect();
    (filtered.len() != args.len()).then_some(filtered)
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

/// The journaled pre-apply lockfile body, when it was captured and is valid UTF-8.
fn journaled_lock<L: NodeLock>(journal: &ProjectMutationJournal) -> Option<&str> {
    journal
        .files
        .iter()
        .find(|file| file.path == Utf8Path::new(L::LOCKFILE))
        .and_then(|file| file.contents.as_deref())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
}

/// Everything the peer-feasibility gate reads from one immutable pre-apply lock, gathered once
/// per apply so no evidence source is parsed twice (the member index in particular is shared
/// between importer attribution and the workspace-manifest peer source).
struct PeerEvidence {
    /// Peer contracts recorded in the lock's resolved-package entries.
    requirements: Vec<crate::lock::PeerRequirement>,
    /// Importer attribution: which member declares which `(name, version)`.
    members: crate::lock::MemberIndex,
    /// Names *declared* at multiple versions across importers.
    multi_version: HashSet<String>,
    /// Names *resolved* at multiple versions anywhere in the graph.
    resolved_splits: HashSet<String>,
    /// Peer contracts read from workspace member manifests (empty without a project root).
    workspace: Vec<WorkspacePeer>,
    /// The physical install layout, when the lock records one (npm). Peer visibility is then
    /// judged by nearest-ancestor lookup instead of declaring-member overlap: hoisting lets
    /// disjoint members' packages meet at the root `node_modules`.
    install: Option<crate::lock::InstallPaths>,
}

impl PeerEvidence {
    /// Gathers the gate's evidence from the journaled pre-apply lock. `root` locates the member
    /// manifests for the workspace source; `None` (lock-only contexts) leaves that source empty.
    /// The multi-version sets are computed only when some contract could hold, so the common
    /// no-peers apply skips those lock walks entirely.
    fn gather<L: NodeLock>(root: Option<&Utf8Path>, lock: Option<&str>) -> Self {
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
struct PeerPartition {
    /// The plan with only the changes the resolve may attempt; every other plan setting passes
    /// through untouched.
    retained: Plan,
    /// One [`SkipReason::PeerHeld`] row per held change, naming the blocking dependent and its
    /// verbatim range.
    skipped: Vec<Skipped>,
}

/// The peer-feasibility gate: split the plan into the changes the resolve may attempt and the
/// cross-major changes a lock-recorded peer requirement structurally excludes, each held as a
/// [`SkipReason::PeerHeld`] naming the dependent and its verbatim range.
///
/// pnpm only *warns* on a peer mismatch by default, and npm — which rejects with `ERESOLVE` by
/// default — commits it under relaxed enforcement (`legacy-peer-deps`, common in project
/// `.npmrc`s), so without this gate a cross-major move that breaks a still-present dependent's
/// peer contract (`fumadocs-core` 16→17 under `fumadocs-mdx`'s `fumadocs-core@^16`) can resolve
/// "successfully" and land the break silently. Holding it up front makes `upgrade` skip it with
/// the range named, and `outdated`'s verification reclassify the row `blocked by <dependent>` —
/// the two commands agree.
///
/// The gate only fires when the violation is demonstrable, and fails open everywhere else:
///
/// - only forward moves across an npm compatibility line are gated — the same
///   [`major_key`](version::major_key) predicate that gates `--major`, so `0.1 → 0.2` counts
///   (caret semantics make the 0.x minor the breaking axis). The line is judged from the versions,
///   never the caller-supplied kind, which `outdated`'s verification passes neutrally; in-range
///   moves are the resolver's business;
/// - the range must demonstrably bind: it *matches* the current version and *provably excludes*
///   the target, judged with the tri-state [`range_match`](version::range_match) (peer ranges
///   routinely union majors — `^7.0.0 || ^8.0.0`). A range with any branch the matcher cannot
///   represent (npm hyphen ranges, `workspace:*`) yields `Unknown`, never `Excludes`, so it cannot
///   block;
/// - only an importer-declared (direct) dependent gates. A *transitive* dependent can be floated by
///   the resolver within its parents' ranges to a sibling version that admits the target (npm does
///   exactly this when a peer conflict arises), so its lock-recorded peer range is not
///   authoritative — and a float that would need a still-cooling version is already stopped by the
///   transitive cooldown gate. A direct dependent has no such latitude: an in-range version that
///   lifts the peer would itself have been planned as a move (and then the co-move rule applies);
/// - npm's package-lock attributes importer declarations by *name* only, but its physical layout
///   is instance-exact: a declaring member's own nearest-ancestor lookup identifies its direct
///   copy ([`direct_dependent_members`]), so a name resolved at several versions still gates
///   through the proven-direct instance — and only through it. Without a physical layout (an npm
///   v1 lock) the split stays ambiguous and never gates (pnpm's attribution is version-exact and
///   unaffected);
/// - on a manager with a whole-graph resolve (pnpm), the dependent must not itself be moving in
///   the dependent's own importing context — its target may lift the peer range, and the joint
///   resolve settles the pair's peer contexts in one pass. Holds are recomputed to a fixed point,
///   so a dependent that is itself peer-held (or destined for the resolve's own multi-version
///   skip) stops exempting the packages it pins. Two co-moving packages that each block the
///   other's old range are deliberately both exempt: that joint move is exactly what the joint
///   resolve exists to judge. A manager on the sequential per-package path (npm) grants NO co-move
///   exemption: its resolver never judges the pair jointly — the moves land one at a time, so an
///   exempted package could land against the dependent's *old* range with only a warning. The
///   package stays held while the dependent moves; the dependent's own landing is post-verified
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
///   the package is symlinked (`link:`) or injected (`file:`). A workspace-local dependent binds
///   in its own directory and in every importer that consumes or declares it, and it never moves
///   in a plan, so no co-move exemption applies — only editing its peer range lifts the hold.
///   Cooldown never edits it: a `peerDependencies` entry is a contract the package publishes to
///   its consumers, not a declaration of what it installs, so manifest widening leaves that field
///   alone entirely (`WIDENABLE_FIELDS`) and the workspace-manifest hold covers every provable
///   break rather than only cross-line ones ([`workspace_peer_hold`]). Together those make the
///   contract immutable for the whole apply, which is what lets the pre-apply snapshot serve as
///   the post-resolve verifier's evidence without going stale.
fn partition_peer_held<L: NodeLock>(plan: &Plan, evidence: &PeerEvidence) -> PeerPartition {
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
        // The changes whose own move may exempt their dependents this round. Only a manager with a
        // whole-graph resolve gets the exemption at all — the sequential per-package path never
        // judges a pair jointly, so there `moving` stays empty and every binding range holds. One
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

/// The `peer_held` skip row for `change`, blocked by `blocker`'s recorded contract. `offending`
/// names the party whose move (or range edit) would release the hold: the dependent when the
/// change is the peer target, the peer package when the change is the dependent itself (the
/// post-apply verification path). One constructor, so the two paths cannot drift in wording.
fn peer_held_skip<L: NodeLock>(
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
fn settle_landed_candidate<L: NodeLock>(
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
            restore_after_owned_step(candidate_journal, &project.root, candidate_postimage)?;
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
/// violated peer package, and the recorded range. The dependent's version is part of the identity
/// deliberately — two same-named dependent copies (a split held at 1.x in one importer and
/// floated to 3.x in another, both recording the same range) are distinct instances whose
/// violations must not collapse onto one key, or the float's fresh break inherits the old break's
/// grandfathering ([`baseline_covers`] handles the version-insensitive part of "already broken").
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PeerViolationId {
    dependent: String,
    dependent_version: String,
    package: String,
    range: String,
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
type PeerViolations = BTreeMap<PeerViolationId, ViolationDetail>;

/// The contexts behind one proven violation: each binding member paired with the peer version its
/// own context demonstrably binds — resolved from the member's dependent instance on npm's
/// physical tree, or from the importer's own declaration on pnpm. Coverage compares the members
/// ([`baseline_covers`]); attribution judges each binding counterfactually
/// ([`attribute_peer_violation`]).
#[derive(Clone)]
struct ViolationDetail {
    bindings: Vec<(String, String)>,
}

/// Whether the baseline already contained `fresh` in every context it binds: a violation of the
/// same contract *shape* — dependent, package, range, the dependent's exact version aside — with
/// a binding in each of the fresh members. Version-insensitive on purpose (on both sides): a
/// dependent re-recorded at a new patch, or a peer floated between two versions the range
/// excludes either way, keeps an already-broken contract broken — nothing newly breaks, and
/// re-attributing it would reject an innocent candidate. Member-sensitive on purpose: a
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
struct PeerBaseline {
    violations: PeerViolations,
    members: crate::lock::MemberIndex,
    install: Option<crate::lock::InstallPaths>,
    ranges: HashMap<(String, String), String>,
}

impl PeerBaseline {
    fn gather<L: NodeLock>(before: Option<&str>, workspace: &[WorkspacePeer]) -> Self {
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
    /// (npm). `None` when the context did not demonstrably bind it, which leaves the
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

/// Which candidate one fresh binding proves culpable. The after-lock only proves the *pair*
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
/// pin, the transitive age floor, and the workspace-manifest peer contracts the lock is not
/// authoritative for.
#[derive(Clone, Copy)]
struct JointResolve<'a> {
    plan: &'a Plan,
    journal: &'a ProjectMutationJournal,
    multi_version: &'a HashSet<String>,
    window_minutes: Option<i64>,
    workspace: &'a [WorkspacePeer],
}

/// One candidate rejection a verification round decided: which active change to drop, the
/// violated contract, and the blamed other party.
struct PeerRejection {
    index: usize,
    violation: crate::lock::PeerRequirement,
    offending: String,
}

/// Plans this round's rejections from the fresh violations: every candidate a violation *uniquely*
/// proves culpable is rejected in one round — each proof is counterfactual against the immutable
/// baseline, so it stands regardless of the other rejections. A violation touching an
/// already-rejected candidate is a cascade: that party reverts with the journal restore, so it is
/// re-judged after the re-resolve instead of guessed at now. A violation binding in several
/// contexts must prove ONE candidate culpable across all of them; disagreement — like an
/// unattributable binding — aborts the round (`Err`), handing the interaction to the caller's
/// candidate isolation.
fn plan_peer_rejections(
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
        // name-only matching could blame a same-named change in an unrelated member. A
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
/// dependent demonstrably sees. A violation is counted only on proof:
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
/// manifests ([`workspace_peer_requirements`]). They are judged by the same proof rules, bound
/// through each contract's [`origin`](WorkspacePeer::origin) directory (npm) or its importer
/// contexts (pnpm), and a local package never moves in a plan — so a resolver move that breaks one
/// is the moved package's fault, and an unplanned collateral move that breaks one is escalated to
/// candidate isolation.
///
/// A lock with no recorded peer contracts (yarn, bun) and no workspace contracts returns empty
/// without any further parsing.
fn proven_peer_violations<L: NodeLock>(
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
            // physically. One context, exactly identified: no same-named package elsewhere in the
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
fn first_new_peer_violation<L: NodeLock>(
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
/// physical layout can resolve the split instance-exactly (see [`direct_dependent_members`]). An
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
/// context exact rather than name-keyed. It is where the package physically lives, and therefore
/// where its `require`/peer lookups start: node resolves a symlinked workspace package from its
/// real path (`--preserve-symlinks` is off by default), so a consumer's hoisted alias directory is
/// NOT a second binding context — the package always sees its own nested copies first.
///
/// `contexts` are the importer paths the contract binds in on a declaration-isolated layout
/// (pnpm): the package's own directory plus every importer that consumes or declares it.
struct WorkspacePeer {
    requirement: crate::lock::PeerRequirement,
    origin: String,
    contexts: Vec<String>,
}

/// Reads every workspace member manifest and returns its declared peer requirements with the
/// contexts they bind in (see [`WorkspacePeer`]). A member manifest that is missing, unreadable,
/// or nameless is skipped — fail open, the shared rule for every evidence source of this gate. A
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
/// proof. A workspace-local dependent never moves in a plan (it is consumed locally — symlinked or
/// injected — not resolved from the registry), so no co-move exemption applies: only a deliberate
/// edit of its own peer range lifts the hold.
///
/// Unlike [`peer_hold`] this does NOT require the move to cross a compatibility line. A published
/// peer contract is the one range cooldown will not rewrite for the author
/// ([`WIDENABLE_FIELDS`](manifest) omits `peerDependencies`), so the gate must catch *every*
/// provable break or a same-line move would land against a contract that still excludes it: a
/// narrow author-written bound (`>=5.6.0 <5.6.2`) is violated by a mere patch bump, which no
/// compatibility-line test sees. The range proof is unchanged and remains the real test — it must
/// match the current version and provably exclude the target — so widening the trigger can only
/// add holds for demonstrable violations.
fn workspace_peer_hold<'a>(
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
/// another subtree, so a dependent bound to the latter must not hold the former. A change without
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
/// `--major` does not gate inside it. The refinement only widens where the gate still demands
/// proof (a range matching the current version and provably excluding the target), so it can add
/// holds only for true violations. An unparsable version never crosses (fail open).
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
/// (see [`partition_peer_held`] for the gating rules). Blame is deterministic: the first blocker by
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
/// the direct instance, `None` otherwise. Shared by the pre-apply gate
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

/// Whether a change's member attribution overlaps the dependent's declaring importers. A change
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
/// key in the resolved graph is mutually exclusive with whatever peer the resolver did pick. The named
/// blocker is the unique *other* package that carries a peer-suffixed key — the sibling whose peer
/// choice excluded `held`. When blame is ambiguous (no peer-suffixed sibling, or several) it returns
/// `None`, so the caller falls back to the generic "the resolver rejected this change" message — the
/// same best-effort contract as uv's `unique_edge_requirer`.
fn peer_conflict_blocker(lock: &str, held: &str) -> Option<String> {
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

/// Every `packages:` key in a `pnpm-lock.yaml` that carries a `(…)` peer disambiguation suffix —
/// returned as the package names used to attribute a held peer conflict to the sibling that forced
/// the peer choice.
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
            in_packages = line.starts_with("packages:");
        }
    }
    out
}

async fn run_candidate_landing_with<C: CandidateCommand>(
    command: &C,
    project: &Project,
    candidate_journal: &ProjectMutationJournal,
    landing: &CandidateLanding,
) -> Result<OwnedStep> {
    let authorized_baseline = candidate_journal.state_for(landing.authorized_manifests())?;
    let first_result = command
        .run_candidate(&project.root, landing.command())
        .await;
    let first = OwnedStep::capture(first_result, candidate_journal, &project.root)?;
    let (attempt, retried_without_cutoff) = match first.result {
        Ok(()) => (first, false),
        Err(error) => {
            let fallback = matches!(&error, CoreError::Tool { .. })
                .then(|| without_before(landing.command()))
                .flatten();
            if let Some(fallback) = fallback {
                // An existing, baselined post-cutoff package can make npm's historical-tree
                // resolve impossible.
                // Restore the candidate baseline and reapply cooldown's authorized manifests
                // before retrying without the native cutoff.
                restore_after_owned_step(candidate_journal, &project.root, &first.postimage)?;
                landing
                    .authorized_manifests()
                    .restore_if_unchanged(&project.root, &authorized_baseline)?;
                let fallback_result = command.run_candidate(&project.root, &fallback).await;
                (
                    OwnedStep::capture(fallback_result, candidate_journal, &project.root)?,
                    true,
                )
            } else {
                (
                    OwnedStep {
                        result: Err(error),
                        postimage: first.postimage,
                    },
                    false,
                )
            }
        }
    };
    match (&attempt.result, landing) {
        (
            Ok(()),
            CandidateLanding::PinRestoreResync {
                authorized_manifests,
                resync,
                ..
            },
        ) => {
            let authorized_postimage = attempt.postimage.state_for(authorized_manifests)?;
            restore_after_owned_step(authorized_manifests, &project.root, &authorized_postimage)?;
            // The resync must use the same resolver regime as the successful pin.
            // Restoring `--before` here would recreate the historical-tree failure that triggered
            // fallback.
            let resync = if retried_without_cutoff {
                without_before(resync).unwrap_or_else(|| resync.clone())
            } else {
                resync.clone()
            };
            let result = command.run_candidate(&project.root, &resync).await;
            OwnedStep::capture(result, candidate_journal, &project.root)
        }
        _ => Ok(attempt),
    }
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
        let native_snapshot = ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(
                &project.root,
                &native_rel,
            )?],
        };
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

/// The window as whole minutes for pnpm's `minimumReleaseAge`, or `None` for a window that can't be
/// a rolling minute count (an absolute freeze, an opt-out, or zero).
fn window_minutes(spec: &WindowSpec) -> Option<i64> {
    match spec {
        WindowSpec::MinAge(duration) => {
            let minutes = duration.as_secs() / 60;
            (minutes > 0).then_some(minutes)
        }
        WindowSpec::Freeze(_) | WindowSpec::Latest => None,
    }
}

/// Set a top-level scalar `key: value` in a YAML file, preserving comments and order, writing only
/// when it changes (idempotent). pnpm settings are top-level scalars, so a line-level edit suffices
/// and avoids a full YAML round-trip that would drop comments; a missing file is created.
///
/// Under `dry_run` the file is never written (nor created); the return value still reports whether
/// it would have changed.
fn set_yaml_scalar(path: &Utf8Path, key: &str, value: &str, dry_run: bool) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CoreError::Filesystem(format!("{path}: {e}"))),
    };
    let target = format!("{key}: {value}");
    let prefix = format!("{key}:");
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    let mut changed = false;
    for line in content.lines() {
        // A top-level key has no leading indentation; the `:` in the prefix avoids matching a
        // longer key with the same start (e.g. `minimumReleaseAgeExclude`).
        if !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            if line == target {
                lines.push(line.to_string());
            } else {
                changed = true;
                lines.push(target.clone());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        if !dry_run {
            // Prepend the setting as a new top-level key, keeping the existing document below it.
            let mut out = target;
            out.push('\n');
            out.push_str(&content);
            std::fs::write(path, out).map_err(|e| CoreError::Filesystem(format!("{path}: {e}")))?;
        }
        return Ok(true);
    }
    if changed && !dry_run {
        let mut out = lines.join("\n");
        if content.ends_with('\n') {
            out.push('\n');
        }
        std::fs::write(path, out).map_err(|e| CoreError::Filesystem(format!("{path}: {e}")))?;
    }
    Ok(changed)
}

/// Set a top-level YAML block sequence (`key:\n  - item\n  - item`) in a file, preserving comments
/// and the rest of the document, writing only when it changes (idempotent). An empty `items` removes
/// the key and its block entirely, so the native config never carries an empty exemption list (and a
/// package toggled back under the cooldown cleans up after itself). Items are emitted as double-quoted
/// scalars — safe for scoped names (`@scope/pkg`) and glob patterns (`@scope/*`) — in the order given
/// (the caller sorts them for determinism). A missing file with non-empty `items` is created.
///
/// Under `dry_run` the file is never written; the return value still reports whether it would change.
fn set_yaml_block_list(
    path: &Utf8Path,
    key: &str,
    items: &[String],
    dry_run: bool,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CoreError::Filesystem(format!("{path}: {e}"))),
    };

    // The canonical block we want, or empty when there are no items (the key is then absent).
    let desired: Vec<String> = if items.is_empty() {
        Vec::new()
    } else {
        std::iter::once(format!("{key}:"))
            .chain(items.iter().map(|item| format!("  - \"{item}\"")))
            .collect()
    };

    let prefix = format!("{key}:");
    let mut out: Vec<String> = Vec::new();
    let mut existing: Vec<String> = Vec::new();
    let mut found = false;
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        // A top-level key has no leading indentation; its block is the following indented lines.
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            existing.push(line.to_string());
            while lines
                .peek()
                .is_some_and(|next| next.starts_with(char::is_whitespace))
            {
                existing.push(lines.next().unwrap_or_default().to_string());
            }
            // Splice the desired block where the old one was (or drop it when empty).
            out.extend(desired.iter().cloned());
        } else {
            out.push(line.to_string());
        }
    }

    let changed = if found {
        existing != desired
    } else {
        !desired.is_empty()
    };
    if !changed || dry_run {
        return Ok(changed);
    }

    let mut text = if found {
        out.join("\n")
    } else {
        // Append the new block after the existing document (e.g. below `minimumReleaseAge`).
        let mut text = content.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&desired.join("\n"));
        text
    };
    if content.ends_with('\n') || !found {
        text.push('\n');
    }
    std::fs::write(path, text).map_err(|e| CoreError::Filesystem(format!("{path}: {e}")))?;
    Ok(true)
}

/// Set a top-level YAML string map while preserving the rest of the document.
///
/// The repair path uses this only for a temporary `overrides` map and restores the original bytes
/// before returning, so comments inside the original block reappear unchanged.
fn set_yaml_string_map(
    path: &Utf8Path,
    key: &str,
    items: &BTreeMap<String, String>,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CoreError::Filesystem(format!("{path}: {e}"))),
    };
    let mut desired = Vec::new();
    if !items.is_empty() {
        desired.push(format!("{key}:"));
        for (item_key, value) in items {
            let item_key = serde_json::to_string(item_key)
                .map_err(|e| CoreError::Serialization(e.to_string()))?;
            let value = serde_json::to_string(value)
                .map_err(|e| CoreError::Serialization(e.to_string()))?;
            desired.push(format!("  {item_key}: {value}"));
        }
    }

    let prefix = format!("{key}:");
    let mut out = Vec::new();
    let mut existing = Vec::new();
    let mut found = false;
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            existing.push(line.to_string());
            while lines
                .peek()
                .is_some_and(|next| next.starts_with(char::is_whitespace))
            {
                existing.push(lines.next().unwrap_or_default().to_string());
            }
            out.extend(desired.iter().cloned());
        } else {
            out.push(line.to_string());
        }
    }

    let changed = if found {
        existing != desired
    } else {
        !desired.is_empty()
    };
    if !changed {
        return Ok(changed);
    }

    let mut text = if found {
        out.join("\n")
    } else {
        let mut text = content.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&desired.join("\n"));
        text
    };
    if content.ends_with('\n') || !found {
        text.push('\n');
    }
    std::fs::write(path, text).map_err(|e| CoreError::Filesystem(format!("{path}: {e}")))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{Npm, Pnpm};
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
        let journal = ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(root, relative)?],
        };
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
        let candidate = ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(root, relative)?],
        };
        std::fs::write(root.join(relative), authorized_manifest)?;
        let authorized = ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(root, relative)?],
        };
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
