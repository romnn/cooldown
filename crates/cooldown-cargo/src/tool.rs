//! The Rust/Cargo [`Tool`]: detection, the resolved graph via `cargo metadata`, classified
//! releases from the crates.io sparse index, and `cargo`-driven apply/build.
//!
//! Cargo has no publish-date cutoff flag (no `--exclude-newer` equivalent), so the cooldown window
//! is realized entirely in [`cooldown_core`].
//! The crates.io sparse index supplies publish times, the core computes each crate's
//! newest-within-window target, and this adapter applies those targets as concrete
//! `cargo update --precise <version>` pins.
//!
//! Apply re-resolves the **whole** graph by issuing all planned pins as one logical unit.
//! Each target gets its own command because Cargo silently applies only the first package spec when
//! several share one `--precise` argument.
//! Version reporting compares before/after `Cargo.lock` slots, while edge reporting audits moves
//! paired across stable dependent identities whose endpoints coexist in both snapshots.
//! The slot comparison reports planned and collateral version changes, while a planned candidate
//! that does not reach its target receives a held row.
//! Changed dependent identities and unpaired entries remain package-set changes rather than
//! attributable binding rows.
//! A converged graph re-applies to a byte-stable fixed point.

use crate::CARGO_ID;
use crate::cargocmd::{Cargo, ResolvedGraph};
use crate::edges;
use crate::index::{CRATES_IO, CratesIoIndex};
use crate::lockfile::{CargoLock, SlotKey, SourcedSlotKey};
use crate::manifest;
use crate::native::parse_native;
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{
    RegistryVersionClassifier, build_registry_releases, verify_current_report,
};
use cooldown_core::{
    ApplyAttempt, ApplyObserver, ApplyReport, Capabilities, Change, CoreError, DepScope,
    Dependency, EdgeNormalizationReport, EdgePolicy, EdgeRebind, FetchContext, LockVerifyReport,
    MemberRef, MutationExecution, NativePolicyLayer, PackageId, PackageRegistry, Plan,
    PreparedMutation, Project, ProjectMarker, ProjectMutationFile, ProjectMutationJournal, Release,
    ReleaseFetcher, ReleaseOrder, ReleaseQuality, Result, RewriteMode, SkipReason, Skipped, ToolId,
    ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use std::collections::{BTreeMap, BTreeSet};

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

    pub(crate) const fn cargo(&self) -> &Cargo {
        &self.cargo
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

    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        // A `Cargo.lock` marks a workspace root: `cargo metadata` there already covers every
        // member, so nested lockfiles below it are not separate projects.
        cooldown_core::ProjectDetection::PrimaryWithValidation {
            primary: ProjectMarker {
                lockfile: "Cargo.lock",
                manifest: "Cargo.toml",
                alternate_manifests: &[],
                workspace_root: true,
            },
            validation_marker: "Cargo.toml",
        }
    }

    fn validate_manifests_without_lock(&self, roots: &[Utf8PathBuf]) -> Result<()> {
        crate::staging::reject_custom_lockfiles(roots)
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        crate::staging::reject_custom_lockfile(&project.root)?;
        edges::enforce::ensure_no_pending(project)?;
        let graph = self.cargo.metadata_locked(&project.root).await?;
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
                declared_bound: graph
                    .declared_bound(&info.name, &info.version)
                    .map(str::to_string),
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
        crate::staging::reject_custom_lockfile(&project.root)?;
        edges::enforce::ensure_no_pending(project)?;
        match self.cargo.verify_locked(&project.root).await {
            Ok(graph) => Ok(verify_current_report(
                graph.is_some(),
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
                changes.push(collateral_change(&registry, name, from, to));
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
fn collateral_change(registry: &str, name: &str, from: &str, to: &str) -> Change {
    let downgrade = version::compare(to, from).is_lt();
    Change {
        package: PackageId::new(CARGO_ID, name.to_string(), Some(registry.to_string())),
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

/// Pin-rejection diagnostics keyed by planned-change identity. Keying by name alone would let
/// coexisting-major plans for one crate (cargo forks distinct majors side by side) overwrite each
/// other's diagnostics and attach one target's rejection to the other's held row.
type PinRejections = BTreeMap<(String, String, String), String>;

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
            report.skipped.push(Skipped {
                change: change.clone(),
                reason: SkipReason::ResolverConflict,
                offending: Some(PackageId::new(
                    CARGO_ID,
                    offender,
                    Some(CRATES_IO.to_string()),
                )),
                // Cargo's own rejection sentence (which requirement, declared by whom) beats
                // both the generic reason and the `=`-pin-only graph offender.
                detail: pin_rejections.get(&rejection_key(change)).cloned(),
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
) -> Result<Vec<(Utf8PathBuf, Option<String>)>> {
    let mut paths: Vec<Utf8PathBuf> = members
        .iter()
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
) -> Result<()> {
    for (rel, contents) in snapshot {
        if let Some(contents) = contents {
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
    /// Re-resolves the **whole** graph under cooldown's window.
    ///
    /// `upgrade` is informational for the rewrite policy.
    /// Cargo has no date cutoff, so each planned target is expressed as a concrete `--precise` pin
    /// computed by the core.
    /// Under `Always`, every owning constraint is widened up front, before the pin batch. Under
    /// `Auto`, the pin batch runs first and only the candidates it left short of a cross-major
    /// target get a *tentative* post-pin widen — kept when the re-pin lands, restored when it
    /// does not (see the loop below).
    async fn whole_graph_resolve(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
    ) -> Result<PinRejections> {
        // Widen the owning manifest constraints for all candidates up front under `Always`; under
        // `Auto`, widen only those whose own declared requirement would otherwise cap them below the
        // target (a cross-major bump). The pin itself follows.
        if matches!(plan.rewrite, RewriteMode::Always) {
            for change in &plan.changes {
                journal.validate_project(&project.root)?;
                manifest::widen_constraint(
                    &project.root,
                    &change.members,
                    &change.package.name,
                    change.to.as_str(),
                )?;
            }
        }
        let mut rejections = BTreeMap::new();
        self.pin_batch(project, &plan.changes, journal, observer, &mut rejections)
            .await?;

        if matches!(plan.rewrite, RewriteMode::Auto) {
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
            // graph, so skip the `cargo metadata` spawn entirely when no candidate is a direct
            // member dep. When it is needed, fail closed: falling back to the lock-slot check is the
            // false positive this loop exists to avoid.
            let needs_graph = plan.changes.iter().any(needs_member_graph);
            for _ in 0..plan.changes.len() {
                let after = read_lock(project)?.crates_io_locked_versions();
                let graph = if needs_graph {
                    journal.validate_project(&project.root)?;
                    Some(self.cargo.metadata(&project.root).await?)
                } else {
                    None
                };
                let mut progressed = false;
                let mut unwidened_short: Vec<Change> = Vec::new();
                for change in &plan.changes {
                    if reached_after(&after, graph.as_ref(), change) {
                        continue;
                    }
                    // Captured before the widen — the first mutation the rollback must undo; the
                    // pin and the metadata probe below both mutate the staged lock the snapshot
                    // carries.
                    let snapshot = widen_snapshot(&project.root, &change.members)?;
                    journal.validate_project(&project.root)?;
                    if manifest::widen_constraint(
                        &project.root,
                        &change.members,
                        &change.package.name,
                        change.to.as_str(),
                    )?
                    .modified
                    .is_empty()
                    {
                        unwidened_short.push(change.clone());
                        continue;
                    }
                    self.pin_batch(
                        project,
                        std::slice::from_ref(change),
                        journal,
                        observer,
                        &mut rejections,
                    )
                    .await?;
                    // The unlocked metadata resolve runs *before* the lock re-read: it may itself
                    // re-lock the widened requirement — including forking a new major alongside a
                    // third-party-held old one, which `update --precise` cannot express — and the
                    // reach check must see that result. A resolve the widened requirement makes
                    // unsatisfiable is this candidate's rejection, not a batch failure: record
                    // cargo's explanation, restore the widen, and move on.
                    let landed_graph = if needs_member_graph(change) {
                        journal.validate_project(&project.root)?;
                        match self.cargo.metadata(&project.root).await {
                            Ok(graph) => Some(graph),
                            Err(err)
                                if err.is_tool_spawn_failure()
                                    || err.is_local_environment_failure() =>
                            {
                                return Err(err);
                            }
                            Err(err) => {
                                if let Some(summary) = summarize_pin_rejection(&err) {
                                    rejections.entry(rejection_key(change)).or_insert(summary);
                                }
                                restore_widen_snapshot(&project.root, &snapshot)?;
                                continue;
                            }
                        }
                    } else {
                        None
                    };
                    let landed = read_lock(project)?.crates_io_locked_versions();
                    if reached_after(&landed, landed_graph.as_ref(), change) {
                        progressed = true;
                    } else {
                        restore_widen_snapshot(&project.root, &snapshot)?;
                    }
                }
                // A sibling's landed widen+pin may have removed the shared blocker behind an
                // unwidened candidate's recorded rejection, so give those candidates their re-pin
                // (the batch skips any node already at target) before the next round decides they
                // are conflicted.
                if progressed && !unwidened_short.is_empty() {
                    self.pin_batch(
                        project,
                        &unwidened_short,
                        journal,
                        observer,
                        &mut rejections,
                    )
                    .await?;
                }
                if !progressed {
                    break;
                }
            }
        }
        Ok(rejections)
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
    async fn pin_batch(
        &self,
        project: &Project,
        changes: &[Change],
        journal: &ProjectMutationJournal,
        observer: Option<&dyn ApplyObserver>,
        rejections: &mut PinRejections,
    ) -> Result<()> {
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
            let before = read_lock(project)?.locked_slots();
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
                if let Some(observer) = observer {
                    observer.candidate_started(change);
                }
                journal.validate_project(&project.root)?;
                if let Some(rejection) = self
                    .update_precise(project, &change.package.name, &current, change.to.as_str())
                    .await?
                {
                    // Last rejection wins; a candidate a later pass still lands never reads its
                    // stale entry (details are consulted only for unreached candidates).
                    rejections.insert(rejection_key(change), rejection);
                }
            }
            let after = read_lock(project)?.locked_slots();
            if !attempted || after == before {
                break;
            }
        }
        Ok(())
    }

    /// Issues one tolerant precise pin, separating resolver rejection from local breakage.
    ///
    /// A rejected precise pin is a resolver outcome the final lock diff reports as held; the
    /// returned summary of cargo's own explanation lets the skip row name the blocking requirement
    /// instead of the generic "the resolver rejected this change". Broken local state must
    /// propagate; otherwise disk-full or spawn failures would masquerade as a conflict in the
    /// candidate set.
    async fn update_precise(
        &self,
        project: &Project,
        name: &str,
        from: &str,
        to: &str,
    ) -> Result<Option<String>> {
        match self
            .cargo
            .update_precise_crates_io(&project.root, name, from, to)
            .await
        {
            Ok(()) => Ok(None),
            Err(err) if err.is_local_environment_failure() => Err(err),
            Err(err) => Ok(summarize_pin_rejection(&err)),
        }
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
        let before_lock = journal
            .files()
            .iter()
            .find(|file| file.path() == Utf8Path::new("Cargo.lock"))
            .and_then(ProjectMutationFile::contents)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|content| CargoLock::parse(content).ok());
        let before = before_lock
            .as_ref()
            .map(CargoLock::locked_versions_by_source)
            .unwrap_or_default();

        // The whole graph is re-resolved as one logical batch: each concrete pin gets its own Cargo
        // invocation, repeated to a bounded fixed point because one invocation may move a package
        // another planned pin still needs to address.
        // The diff below surfaces every paired version-slot move.
        let pin_phase = std::time::Instant::now();
        let pin_rejections = match self
            .whole_graph_resolve(project, plan, journal, observer)
            .await
        {
            Ok(rejections) => rejections,
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
            journal.validate_project(&project.root)?;
            Some(self.cargo.metadata(&project.root).await?)
        } else {
            None
        };

        // Enforce the plan's edge policy over the re-resolved lock and collect the moves observation
        // can pair across stable dependent identities and coexisting endpoints.
        journal.validate_project(&project.root)?;
        let edge_phase = std::time::Instant::now();
        let enforced = edges::enforce::enforce(
            &self.cargo,
            project,
            plan.edge_policy,
            before_lock.as_ref(),
            graph,
        )
        .await?;
        tracing::debug!(
            elapsed_ms = edge_phase.elapsed().as_millis(),
            "cargo edge phase finished"
        );
        let graph = enforced.graph;
        let edge_rebinds = enforced.rebinds;

        let after_lock = read_lock(project)?;
        let after = after_lock.locked_versions_by_source();
        let crates_io_after = after_lock.crates_io_locked_versions();
        classify_planned_changes(
            plan,
            &crates_io_after,
            graph.as_ref(),
            &pin_rejections,
            &mut report,
        );

        // No paired version-slot change may be omitted.
        // Every moved slot the applied rows above do not already report is surfaced as a collateral
        // applied row: a transitive pushed backward for consistency, a crate matured down by `fix`,
        // or a *held* candidate the resolve still floated off its baseline (whose skip row alone
        // would hide that real move).
        let collateral = collateral_changes(&before, &after, &report.applied);
        tracing::debug!(
            before_slots = before.len(),
            after_slots = after.len(),
            collateral = collateral.len(),
            "cargo apply collateral diff"
        );
        report.applied.extend(collateral);
        report.edge_rebinds = edge_rebinds;
        Ok(report)
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

fn needs_apply_graph(changes: &[Change], after: &BTreeMap<SlotKey, String>) -> bool {
    changes
        .iter()
        .any(|change| needs_member_graph(change) || !reached(after, change))
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

    fn supports_transitive_advance(&self) -> bool {
        // The per-spec `update -p name@from --precise to` pin addresses any locked crate, declared
        // or not.
        true
    }

    fn mutation_execution(&self) -> MutationExecution<'_> {
        MutationExecution::Isolated(self)
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
        ProjectMutationJournal::capture(&project.root, relative)
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, journal) = mutation.isolated_parts_for(self)?;
        self.apply_plan(project, plan, journal, None).await
    }

    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        let (project, plan, journal) = mutation.isolated_parts_for(self)?;
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

    async fn recover_pending_mutation(
        &self,
        project: &Project,
        coordination: &cooldown_core::fs::ProjectCoordination,
    ) -> Result<cooldown_core::MutationRecovery> {
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
        let (project, _, _) = mutation.isolated_parts_for(self)?;
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
                    Some(self.cargo.metadata(&project.root).await?)
                } else {
                    None
                }
            }
            // A metadata failure must remain a project error; otherwise requested canonical
            // healing would degrade to a successful no-op.
            EdgePolicy::Canonicalize => Some(self.cargo.metadata(&project.root).await?),
        };
        let mut result =
            edges::enforce::enforce(&self.cargo, project, policy, before_lock.as_ref(), graph)
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
    use async_trait::async_trait;
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use cooldown_adapter_util::skipped_on_apply_error;
    use cooldown_core::CoreError;
    use indoc::{formatdoc, indoc};

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

    struct InPlaceCargoFamilyWriter;

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
            current: Version::new("1.0.200"),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
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
        let collateral = collateral_changes(&before, &after, &applied);
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
        let collateral = collateral_changes(&before, &after, &[]);
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
        let collateral = collateral_changes(&before, &after, &[]);
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
        let collateral = collateral_changes(&before, &after, &applied);
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
        let collateral = collateral_changes(&before, &after, &[]);
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].from.as_str(), "1.2.0");
        assert_eq!(collateral[0].to.as_str(), "1.3.0");
        assert_eq!(
            collateral[0].package.registry.as_deref(),
            Some("registry:registry.example.com"),
            "the row names its own registry, not crates.io"
        );
    }

    #[test]
    fn collateral_changes_surface_a_held_candidates_real_movement() {
        // A held planned candidate has no applied row, yet a sibling pin still floated it off its
        // baseline. That net move must surface as a collateral row beside the held skip instead of
        // being silently dropped behind the planned name.
        let before = lock_with(&[("referencing", "0.46.5")]).locked_versions_by_source();
        let after = lock_with(&[("referencing", "0.46.10")]).locked_versions_by_source();
        let collateral = collateral_changes(&before, &after, &[]);
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "referencing");
        assert_eq!(collateral[0].from.as_str(), "0.46.5");
        assert_eq!(collateral[0].to.as_str(), "0.46.10");

        // Once the candidate reaches its target, its applied row already tells the move: no
        // duplicate collateral row.
        let landed = lock_with(&[("referencing", "0.46.10")]).locked_versions_by_source();
        let applied = [change("referencing", "0.46.5", "0.46.10", false)];
        assert!(collateral_changes(&before, &landed, &applied).is_empty());
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
        dependencies = ["dep"]

        [[package]]
        name = "dep"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
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
            std::fs::write(
                root.join("Cargo.lock.forked"),
                formatdoc! {r#"
                    {WIDEN_LOCK}
                    [[package]]
                    name = "dep"
                    version = "2.0.0"
                    source = "registry+https://github.com/rust-lang/crates.io-index"
                "#},
            )?;
            indoc! {r#"
                if ! grep -q 'dep = "1"' Cargo.toml; then
                  cp Cargo.lock.forked Cargo.lock
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
        };
        Ok((project, tool))
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
    async fn tentative_auto_widen_is_kept_when_the_pin_lands_as_a_fork() -> eyre::Result<()> {
        // The success arm of the same state machine: once the widen lifts the declared cap, the
        // re-pin forks the new major beside the third-party-held old line. The widened manifest
        // must be kept — restoring it would demand the old major back and undo the landing.
        let dir = tempfile::tempdir()?;
        let root = Utf8Path::from_path(dir.path()).ok_or_else(|| eyre::eyre!("non-UTF-8 root"))?;
        let (project, tool) = widen_fixture(root, true)?;
        let plan = Plan {
            changes: vec![change("dep", "1.0.0", "2.0.0", false)],
            rewrite: RewriteMode::Auto,
            ..Plan::default()
        };
        let journal = tool.mutation_journal(&project, &plan).await?;

        tool.whole_graph_resolve(&project, &plan, &journal, None)
            .await?;

        let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
        assert!(
            !manifest.contains(r#"dep = "1""#),
            "the landed widen keeps the manifest on the new requirement: {manifest}"
        );
        let landed = read_lock(&project)?.crates_io_locked_versions();
        assert_eq!(
            landed.get(&("dep".into(), "2".into())).map(String::as_str),
            Some("2.0.0"),
            "the pin landed in the target major slot"
        );
        assert_eq!(
            landed.get(&("dep".into(), "1".into())).map(String::as_str),
            Some("1.0.0"),
            "the third-party-held old major coexists as its own line"
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
