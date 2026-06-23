//! The generic JavaScript/TypeScript [`Tool`]: detection, the resolved graph from a lockfile, npm
//! registry publish times, and driver-backed re-resolution/apply. The lockfile format and driver
//! binary are supplied by a [`NodeLock`] type parameter, so npm, pnpm, yarn, and bun are all the
//! same adapter specialised over their lock format — they share the npm registry and version model
//! and differ only in how their lock is parsed and how their CLI re-pins a dependency.

use crate::lock::NodeLock;
use crate::manifest;
use crate::nodecmd::NodeCmd;
use crate::registry::{NPM, NpmRegistry};
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{
    build_registry_releases, skipped_on_apply_error, verify_current_report,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, CoreError, DepScope, Dependency,
    FetchContext, MemberRef, NativePolicyLayer, PackageId, PackageRegistry, Plan, Project,
    ProjectMarker, ProjectMutationJournal, RawRelease, Release, ReleaseFetcher, ReleaseOrder,
    ReleaseQuality, ResolvedPolicy, Result, RewriteMode, SkipReason, Skipped, SyncReport,
    SyncScope, ToolId, ToolRead, ToolWrite, VerifyReport, Version, WindowSpec,
};
use cooldown_registry::SharedHttp;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::marker::PhantomData;

/// The resolved lock's `name -> version` map, the snapshot `apply` diffs before/after the whole-graph
/// re-resolve so *every* net version change is reported (the planned moves, the collateral churn the
/// joint resolve forced on other packages, and the candidates left held below their target). A name
/// that resolves to several versions (a duplicated graph copy) keeps its newest, so a moved direct
/// declaration is never masked by a stale transitive copy of the same name.
fn locked_versions<L: NodeLock>(content: &str) -> HashMap<String, String> {
    let mut versions: HashMap<String, String> = HashMap::new();
    for (name, version) in L::parse(content).unwrap_or_default() {
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
pub(crate) fn build_releases(current: &str, raw: Vec<RawRelease>) -> Vec<Release> {
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

/// Captures the lockfile and every package manifest this plan could rewrite.
fn journal<L: NodeLock>(project: &Project, plan: &Plan) -> Result<ProjectMutationJournal> {
    let mut seen = BTreeSet::new();
    let mut rels = Vec::new();
    push_journal_rel(&mut rels, &mut seen, Utf8PathBuf::from(L::LOCKFILE));
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
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        // The lockfile sits at the workspace root; nested `package.json`s share it (no nested lock).
        ProjectMarker {
            lockfile: L::LOCKFILE,
            manifest: "package.json",
            workspace_root: true,
        }
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
        for (name, version) in resolved {
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
            let members = member_paths
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
            deps.push(Dependency {
                package: PackageId::new(L::ID, name, Some(NPM.to_string())),
                current: Version::new(version.clone()),
                current_quality: classify_quality(&version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
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

    async fn verify_lock_current(&self, _project: &Project) -> Result<VerifyReport> {
        // The npm-family CLIs lack a cheap, uniform "is the lock current?" probe, so cooldown
        // trusts the committed lock as the source of truth rather than re-resolving on every read.
        Ok(verify_current_report(
            true,
            "lockfile taken as current",
            "lockfile is stale",
        ))
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
        let raw = self.registry.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
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
            kind_from_current: None,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

/// Choose the lock-only driver command for one change, when the package manager supports one.
///
/// In `Auto` mode, when the package manager offers a lock-only update (only pnpm does) and the target
/// already satisfies the declared `package.json` range, move just the lock and leave the range as the
/// author wrote it. Otherwise — `Always`, a manager without a lock-only path, an out-of-range
/// target, or a range we cannot evaluate — the caller rewrites the declaring package manifests and
/// refreshes the lock. The in-range check happens up front because lock-only commands re-pin whatever
/// version they are given without validating it, so an out-of-range version would leave the lock
/// inconsistent with `package.json`.
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

/// The command that refreshes the lock after [`manifest::widen_constraints`] rewrote the declaring
/// manifests for an out-of-range (or `--rewrite`) change.
///
/// Prefer a per-version pin so the lock lands on exactly the cooldown-approved target: a bare
/// `relock_args` install re-resolves the just-widened range to its *newest* member, which can
/// overshoot onto a newer-but-still-too-fresh release that the post-apply cooldown check then rolls
/// back — silently failing a valid upgrade. pnpm pins the exact version without touching any manifest
/// (`update --no-save`). npm's exact pin (`install <name>@<version>`) also saves `^version` to the
/// *root* manifest, so it is used only when the root declares the dependency (the entry we just
/// widened); for a member-only dependency that would add a spurious root dependency, so we re-resolve
/// instead (an overshoot is safely rolled back). yarn and bun have no exact pin and re-resolve too.
fn rewrite_relock<L: NodeLock>(project: &Project, change: &Change) -> Result<Vec<String>> {
    let name = &change.package.name;
    let version = change.to.as_str();
    if let Some(args) = L::lockonly_update_args(name, version) {
        return Ok(args);
    }
    if let Some(args) = L::pinned_relock_args(name, version)
        && manifest::declared_range(&project.manifest, name)?.is_some()
    {
        return Ok(args);
    }
    Ok(L::relock_args())
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

/// Whether a planned candidate landed at or beyond its target in `after`, respecting the move's
/// direction: a forward move must reach at/above its target, a downgrade at/below it. A package the
/// resolve dropped from the lock (no entry) counts as not reached.
fn reached(after: &HashMap<String, String>, change: &Change) -> bool {
    after.get(&change.package.name).is_some_and(|landed| {
        let ordering = version::compare(landed, change.to.as_str());
        if change.downgrade {
            ordering.is_le()
        } else {
            ordering.is_ge()
        }
    })
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
    for (name, _) in pnpm_peer_suffixed_keys(lock) {
        if name != held {
            blockers.insert(name);
        }
    }
    match blockers.len() {
        1 => blockers.into_iter().next(),
        _ => None,
    }
}

/// The `(name, peer-suffix)` of every `packages:` key in a `pnpm-lock.yaml` that carries a `(…)` peer
/// disambiguation suffix — the resolved entries whose identity depends on a peer resolution. Used to
/// attribute a held peer conflict to the sibling that forced the peer choice.
fn pnpm_peer_suffixed_keys(lock: &str) -> Vec<(String, String)> {
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
            let suffix = key[open..].to_string();
            let base = key[..open].to_string();
            if let Some((name, _version)) = crate::lock::split_name_version(&base) {
                out.push((name, suffix));
            }
        } else {
            in_packages = line.starts_with("packages:");
        }
    }
    out
}

impl<L: NodeLock> NpmTool<L> {
    /// The original per-package apply: for each change, move the lock with a lock-only update (pnpm)
    /// or, after widening the declaring `package.json`, a pinned/bare relock. Kept for npm/yarn/bun,
    /// which have neither a window flag nor a single joint resolve, so a batched whole-graph pass would
    /// gain nothing over moving each change in turn.
    async fn apply_per_package(&self, project: &Project, plan: &Plan) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            let args = if let Some(args) = lockonly_command::<L>(project, change, plan.rewrite)? {
                args
            } else {
                let rewrite = manifest::widen_constraints(
                    &project.root,
                    &change.members,
                    &change.package.name,
                    change.to.as_str(),
                )?;
                if rewrite.modified.is_empty() {
                    report.skipped.push(Skipped {
                        change: change.clone(),
                        reason: SkipReason::NotEligible,
                        offending: Some(change.package.clone()),
                    });
                    continue;
                }
                rewrite_relock::<L>(project, change)?
            };
            match self.cmd.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    /// Re-resolve the **whole** importer graph once (pnpm), pinning every planned candidate to its
    /// EXACT per-package target, then report the full before/after lock diff — the proven cargo/go
    /// pattern ported to pnpm.
    ///
    /// One `pnpm update <pkg>@<target> … --lockfile-only --no-save --config.minimumReleaseAge=<minutes>`
    /// jointly re-resolves every importer, settling mutually-exclusive peer conflicts at a single fixed
    /// point instead of ping-ponging between per-package updates. Each `<pkg>@<target>` is the
    /// candidate's own `change.to`, computed by cooldown-core under that package's window, so a package
    /// with a *stricter* per-package window lands at its older per-package target rather than
    /// overshooting onto the global-window-newest — the gap a bare `--latest` left, since pnpm's
    /// `minimumReleaseAge` is a single global knob with no per-package publish-date cutoff. This mirrors
    /// cargo's `update --precise <to>` and go's `get module@<to>`: the per-package target already
    /// encodes the per-package window, so pinning it enforces that window exactly. `minimumReleaseAge`
    /// is still passed as the *transitive* floor (a fresh transitive the pins drag in is capped to the
    /// project-default window, so the uniform-window case lands the same lock as before); transitives
    /// floated past it are reconciled down by the caller's transitive-cooldown gate, exactly as for
    /// cargo/go. The report is the diff of the journal's pre-apply lock against the result, so every
    /// planned candidate is reported reached or held (naming the conflicting peer where attributable)
    /// and every collateral move of an unplanned package surfaces as its own row. A resolver failure
    /// marks all candidates held and lets the caller restore the journal.
    async fn apply_whole_graph(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if plan.changes.is_empty() {
            return Ok(report);
        }

        // The pre-apply lock, captured in the journal. Both the newest-version map (for the move diff)
        // and the multi-version set (for the exact-pin-vs-float decision) are derived from this one
        // copy — no extra disk read, and both see exactly the lock the resolve starts from.
        let before_content = journal
            .files
            .iter()
            .find(|file| file.path == Utf8Path::new(L::LOCKFILE))
            .and_then(|file| file.contents.as_deref())
            .and_then(|bytes| std::str::from_utf8(bytes).ok());
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
        match self
            .whole_graph_resolve(project, plan, &multi_version, window_minutes)
            .await
        {
            Ok(()) => {}
            Err(err) if err.is_tool_spawn_failure() => return Err(err),
            // The joint resolve is unsatisfiable as a whole. Propagate the failure so the caller's
            // `apply_resilient` can isolate the offending candidate(s) (an unfetchable version, one
            // side of a conflict) and apply the rest, instead of holding every candidate. The caller
            // restores the journal, so no partial lock is kept.
            Err(err) => return Err(err),
        }

        let after_content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let after = locked_versions::<L>(&after_content);
        let planned: HashSet<&str> = plan
            .changes
            .iter()
            .map(|change| change.package.name.as_str())
            .collect();

        for change in &plan.changes {
            let name = change.package.name.as_str();
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
            if reached(&after, change) {
                if moved {
                    report.applied.push(change.clone());
                }
                // Reached its target without a net lock move — already satisfied (a duplicate copy of
                // the same name is at the target). A no-op, neither applied nor held.
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
                });
            }
        }

        let mut collateral: Vec<Change> = before
            .iter()
            .filter(|(name, _)| !planned.contains(name.as_str()))
            .filter_map(|(name, from)| {
                let to = after.get(name)?;
                (version::compare(from, to).is_ne()).then(|| collateral_change::<L>(name, from, to))
            })
            .collect();
        collateral.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        report.applied.extend(collateral);
        Ok(report)
    }

    /// Build the per-candidate pins (exact target vs range-floated), widen the manifests the exact
    /// pins need, then run the single joint `--recursive` resolve.
    ///
    /// A candidate held at a single version across the workspace is **exact-pinned** to its
    /// per-package target (`name@target`): the resolve lands it at exactly that version, honoring a
    /// stricter-than-global per-package window with no overshoot. A candidate a member declares at a
    /// version *other* members also hold at a different version (a v4/v5 split, which pnpm keeps like
    /// cargo) is **range-floated** by name instead: exact-pinning one target would collapse every other
    /// copy onto it, so the bare name lets each importer's range re-resolve to its own newest-within-
    /// window line. The pre-apply lock identifies those multi-version names; a missing/unparsable lock
    /// means nothing is multi-version yet, so every pin is exact.
    ///
    /// Widen is for the exact pins only, and only when their target is out of the declared range
    /// (`Auto`) or always (`Always`). It is mandatory there: `pnpm update <pkg>@<target> --no-save`
    /// re-pins the lock to an out-of-range target but leaves the manifest as written, so the next
    /// resolve (which re-resolves any package it is not pinning, against its manifest range) snaps the
    /// candidate back into range and breaks the fixed point. A range-floated candidate is never widened
    /// — widening would let it cross its own range boundary, the very line we are preserving — and it
    /// stays in range by construction, so it re-applies byte-stable.
    ///
    /// `--recursive` is what makes the resolve span the whole workspace: a bare `pnpm update` at the
    /// root only re-pins root-declared dependencies, so a candidate a member declares would never move.
    async fn whole_graph_resolve(
        &self,
        project: &Project,
        plan: &Plan,
        multi_version: &HashSet<String>,
        window_minutes: Option<i64>,
    ) -> Result<()> {
        let mut pins: Vec<(String, Option<String>)> = Vec::with_capacity(plan.changes.len());
        for change in &plan.changes {
            let name = change.package.name.clone();
            if multi_version.contains(&name) {
                // Range-float: preserve every distinct line; never widen (that would cross the range
                // boundary we are keeping).
                pins.push((name, None));
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
                )?;
            }
            pins.push((name, Some(change.to.as_str().to_string())));
        }
        self.joint_resolve(project, &pins, window_minutes).await?;
        // The up-front pass already widened every out-of-range exact target, so a candidate the resolve
        // still left short of its target is blocked by *another* package's requirement (a peer
        // conflict), which widening its own declared range cannot resolve — the lock diff reports it
        // held. No post-resolve re-widen loop is needed.
        Ok(())
    }

    async fn joint_resolve(
        &self,
        project: &Project,
        pins: &[(String, Option<String>)],
        window_minutes: Option<i64>,
    ) -> Result<()> {
        let Some(args) = L::whole_graph_args(pins, window_minutes) else {
            return Ok(());
        };
        self.cmd.run(&project.root, &args).await
    }
}

/// Names a workspace importer DECLARES at more than one distinct version — a genuine v4/v5 split that
/// must be range-floated (exact-pinning one target would collapse the other line), unlike everything
/// else which is exact-pinned.
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
        // A manager with a native joint resolve (pnpm) re-resolves the whole importer graph in one
        // pass and reports the full before/after lock diff, so a candidate can never silently move
        // another package and mutually-exclusive peers settle at a single fixed point. The others
        // (npm/yarn/bun) lack a joint pin-set resolve, so they keep the per-package relock path.
        if L::supports_whole_graph_resolve() {
            self.apply_whole_graph(project, plan, journal).await
        } else {
            self.apply_per_package(project, plan).await
        }
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cmd
            .verify(&project.root, &L::build_args(), "install succeeded")
            .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::{Npm, Pnpm};
    use camino::Utf8PathBuf;
    use indoc::indoc;

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
    fn whole_graph_args_pins_each_per_package_target_only_for_pnpm() {
        // pnpm pins each planned candidate to its EXACT per-package target in one joint resolve, so a
        // stricter-windowed package lands at its own (possibly older) target rather than overshooting.
        // The window rides inline as `minimumReleaseAge`, the floor for any fresh transitive the pins
        // drag in.
        // An exact pin (`Some`) becomes `name@target`; a range-floated multi-version candidate (`None`)
        // becomes the bare name, so each importer's range re-resolves instead of collapsing onto one
        // target. `--recursive` so a candidate a workspace member declares (not the root `package.json`)
        // is actually re-pinned; a bare `pnpm update` at the root would skip it.
        let pins = vec![
            ("eslint".to_string(), Some("9.5.0".to_string())),
            (
                "@typescript-eslint/eslint-plugin".to_string(),
                Some("8.0.0".to_string()),
            ),
            // Held at several versions across importers, so floated by range rather than pinned.
            ("chalk".to_string(), None),
        ];
        assert_eq!(
            Pnpm::whole_graph_args(&pins, Some(20160)),
            Some(vec![
                "update".to_string(),
                "--recursive".to_string(),
                "eslint@9.5.0".to_string(),
                "@typescript-eslint/eslint-plugin@8.0.0".to_string(),
                "chalk".to_string(),
                "--lockfile-only".to_string(),
                "--no-save".to_string(),
                "--config.minimumReleaseAge=20160".to_string(),
            ])
        );
        // No pins (the `fix` reconcile with an empty plan) is a plain re-lock; a true Latest opt-out
        // drops the min-age flag.
        assert_eq!(
            Pnpm::whole_graph_args(&[], None),
            Some(vec!["install".to_string(), "--lockfile-only".to_string()])
        );
        // npm/yarn/bun have no joint resolve, so they keep the per-package path.
        assert!(!Npm::supports_whole_graph_resolve());
        assert!(Pnpm::supports_whole_graph_resolve());
        assert_eq!(Npm::whole_graph_args(&pins, Some(20160)), None);
        assert_eq!(crate::lock::Yarn::whole_graph_args(&pins, None), None);
        assert_eq!(crate::lock::Bun::whole_graph_args(&[], None), None);
    }

    #[test]
    fn locked_versions_keeps_the_newest_copy_of_a_duplicated_name() {
        let lock = "lockfileVersion: '9.0'\n\npackages:\n\n  foo@1.0.0:\n    resolution: {integrity: sha512-a}\n\n  foo@2.0.0:\n    resolution: {integrity: sha512-b}\n\n  bar@3.1.0:\n    resolution: {integrity: sha512-c}\n";
        let versions = locked_versions::<Pnpm>(lock);
        assert_eq!(versions.get("foo").map(String::as_str), Some("2.0.0"));
        assert_eq!(versions.get("bar").map(String::as_str), Some("3.1.0"));
    }

    #[test]
    fn reached_respects_move_direction() {
        let mut after = HashMap::new();
        after.insert("pkg-a".to_string(), "2.0.0".to_string());
        let forward = change("pkg-a", "1.0.0", "2.0.0");
        assert!(reached(&after, &forward));
        let forward_short = change("pkg-a", "1.0.0", "2.1.0");
        assert!(!reached(&after, &forward_short));
        let mut down = change("pkg-a", "3.0.0", "2.0.0");
        down.downgrade = true;
        assert!(reached(&after, &down));
        let mut down_short = change("pkg-a", "3.0.0", "1.0.0");
        down_short.downgrade = true;
        assert!(!reached(&after, &down_short));
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
        assert!(matches!(report, cooldown_core::SyncReport::Written { .. }));
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

    fn project_declaring(spec: &str) -> (tempfile::TempDir, Project) {
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
        (dir, project)
    }

    #[test]
    fn pnpm_uses_lock_only_only_for_in_range_auto() {
        let (_dir, project) = project_declaring("^3.0.0");

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
            crate::lock::Pnpm::relock_args(),
            ["install", "--lockfile-only"]
        );
    }

    #[test]
    fn relock_commands_refresh_locks_without_adding_dependencies() {
        assert_eq!(
            crate::lock::Npm::relock_args(),
            ["install", "--package-lock-only", "--no-audit", "--no-fund"]
        );
        assert_eq!(
            crate::lock::Pnpm::relock_args(),
            ["install", "--lockfile-only"]
        );
        assert_eq!(crate::lock::Yarn::relock_args(), ["install"]);
        assert_eq!(crate::lock::Bun::relock_args(), ["install"]);
    }

    #[test]
    fn rewrite_relock_pins_exact_target_where_supported() {
        // Root declares `nanoid`, so the post-widen relock lands the lock on exactly the
        // cooldown-approved version instead of re-resolving the widened range to a newer member.
        let (_dir, project) = project_declaring("^3.0.0");
        let change = change("nanoid", "3.1.0", "5.1.11");

        // pnpm pins the exact target without touching the manifest.
        assert_eq!(
            rewrite_relock::<crate::lock::Pnpm>(&project, &change).expect("cmd"),
            ["update", "nanoid@5.1.11", "--lockfile-only", "--no-save"]
        );
        // npm pins the exact target via `install <name>@<version>` (the root declares it).
        assert_eq!(
            rewrite_relock::<Npm>(&project, &change).expect("cmd"),
            [
                "install",
                "nanoid@5.1.11",
                "--package-lock-only",
                "--no-audit",
                "--no-fund"
            ]
        );
    }

    #[test]
    fn npm_re_resolves_when_root_does_not_declare_the_dependency() {
        // A member-only dependency: npm's exact pin would save it to the root manifest, adding a
        // spurious root dependency, so it re-resolves the widened range instead (safe — an overshoot
        // is rolled back). The root here declares something else.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "lodash": "^4.0.0" } }"#,
        )
        .expect("write manifest");
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
            exclude_newer: None,
        };
        let change = change("nanoid", "3.1.0", "5.1.11");

        assert_eq!(
            rewrite_relock::<Npm>(&project, &change).expect("cmd"),
            ["install", "--package-lock-only", "--no-audit", "--no-fund"]
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
    fn npm_has_no_lock_only_path_so_always_rewrites() {
        let (_dir, project) = project_declaring("^3.0.0");
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
