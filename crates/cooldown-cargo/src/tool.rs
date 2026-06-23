//! The Rust/Cargo [`Tool`]: detection, the resolved graph via `cargo metadata`, classified
//! releases from the crates.io sparse index, and `cargo`-driven apply/build.
//!
//! Cargo has no publish-date cutoff flag (no `--exclude-newer` equivalent), so the cooldown window
//! is realized entirely in [`cooldown_core`]: the crates.io sparse index supplies publish times, the
//! core computes each crate's newest-within-window target, and this adapter applies those as concrete
//! `cargo update --precise <version>` pins. Apply re-resolves the **whole** graph by batching all of
//! a project's planned pins (one `cargo update -p … --precise V` per distinct target version, since
//! `--precise` takes a single version) into one logical unit, then builds the report from the FULL
//! before/after `Cargo.lock` diff — not from per-change outcomes. So every net version change is
//! surfaced (the planned moves, the collateral moves the re-resolve forces on non-candidate crates,
//! and the candidates a mutually-exclusive `=`-pin or single-major shared transitive leaves held),
//! and a converged graph re-applies to a byte-stable fixed point.

use crate::cargocmd::Cargo;
use crate::index::{CRATES_IO, CratesIoIndex};
use crate::manifest;
use crate::native::parse_native;
use crate::version;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{build_registry_releases, verify_current_report};
use cooldown_core::{
    ApplyReport, Capabilities, Change, DepScope, Dependency, FetchContext, NativePolicyLayer,
    PackageId, PackageRegistry, Plan, Project, ProjectMarker, ProjectMutationJournal, Release,
    ReleaseFetcher, ReleaseOrder, ReleaseQuality, Result, RewriteMode, SkipReason, Skipped, ToolId,
    ToolRead, ToolWrite, VerifyReport, Version,
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
            workspace_root: true,
        }
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
            let graph_floor = if graph.is_graph_held(id) {
                Some(Version::new(info.version.clone()))
            } else {
                None
            };
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

    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport> {
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

/// The resolved registry-crate versions of a `Cargo.lock`, keyed per `(name, major)` slot — the
/// snapshot `apply` diffs before/after the whole-graph re-resolve to report *every* net version
/// change (the planned moves, the collateral moves the resolve forced for consistency, and the
/// candidates a conflict left held). Path/git/workspace packages carry no comparable registry
/// version and are skipped. When two nodes share a `(name, major)` slot (rare; only via distinct
/// `source` registries), the highest version wins so the slot is single-valued.
fn locked_versions(lock: &CargoLock) -> BTreeMap<SlotKey, String> {
    let mut slots: BTreeMap<SlotKey, String> = BTreeMap::new();
    for package in &lock.package {
        let (Some(version), true) = (package.version.as_deref(), package.is_registry()) else {
            continue;
        };
        let key = (package.name.clone(), version::major_key(version).0);
        slots
            .entry(key)
            .and_modify(|existing| {
                if version::compare(version, existing).is_gt() {
                    *existing = version.to_string();
                }
            })
            .or_insert_with(|| version.to_string());
    }
    slots
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
}

impl CargoLock {
    fn parse(content: &str) -> Result<Self> {
        toml::from_str(content)
            .map_err(|err| cooldown_core::CoreError::LockUnreadable(format!("Cargo.lock: {err}")))
    }
}

/// A net version change `apply` derived from the before/after lock diff for a crate the plan did not
/// itself name — collateral movement the whole-graph re-resolve forced. Reported so no crate's
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
    ///
    /// Returns the set of planned candidates that the resolve could place at their target (`reached`).
    /// A candidate left short is a real conflict the diff then reports; widening is bounded so a
    /// candidate held by *another* crate's requirement (not its own declared cap) stops the loop.
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
            for _ in 0..plan.changes.len() {
                let after = locked_versions(&read_lock(project)?);
                let mut widened_any = false;
                let mut short = Vec::new();
                for change in &plan.changes {
                    if reached(&after, change) {
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

    /// Apply all `changes` as one logical unit: collapse those sharing a target version into a single
    /// `cargo update -p A -p B --precise V` call (cargo's `--precise` takes one version but multiple
    /// `[SPEC]`s), and issue the distinct-version groups together. A group cargo rejects (a `=`-pin or
    /// resolver conflict blocks the precise move) is not fatal — the candidate simply stays where the
    /// resolver placed it, and the before/after lock diff reports it as held. Only a `cargo` spawn
    /// failure aborts.
    async fn pin_batch(&self, project: &Project, changes: &[Change]) -> Result<()> {
        let mut by_target: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for change in changes {
            by_target
                .entry(change.to.as_str().to_string())
                .or_default()
                .push((
                    change.package.name.clone(),
                    change.from.as_str().to_string(),
                ));
        }
        for (target, specs) in by_target {
            // A precise group cargo rejected (a `=`-pin or resolver conflict blocked the move) is not
            // fatal: the candidates stay where the resolver placed them and the full-lock diff reports
            // each as held. Only a `cargo` spawn failure aborts.
            if let Err(err) = self
                .cargo
                .update_precise_many(&project.root, &specs, &target)
                .await
                && err.is_tool_spawn_failure()
            {
                return Err(err);
            }
        }
        Ok(())
    }
}

fn read_lock(project: &Project) -> Result<CargoLock> {
    let content = std::fs::read_to_string(project.root.join("Cargo.lock"))?;
    CargoLock::parse(&content)
}

/// Whether a planned candidate landed at or beyond its target in `after`, respecting direction: a
/// forward move must reach the slot at/above its target, a downgrade at/below it. Keyed per
/// `(name, major)` slot; a cross-major move is checked against the target's own major slot.
fn reached(after: &BTreeMap<SlotKey, String>, change: &Change) -> bool {
    let key = (
        change.package.name.clone(),
        version::major_key(change.to.as_str()).0,
    );
    after.get(&key).is_some_and(|landed| {
        let ordering = version::compare(landed, change.to.as_str());
        if change.downgrade {
            ordering.is_le()
        } else {
            ordering.is_ge()
        }
    })
}

#[async_trait]
impl ToolWrite for CargoTool {
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

        // The whole graph is re-resolved in one batched pass: all planned `--precise` pins are applied
        // together (grouped by shared target version), so the resolver settles every conflict in one
        // consistent lock. A converged graph re-pins to a byte-stable fixed point — no per-package
        // sequential re-locks, no oscillation, and no crate left to drift unreported because the diff
        // below surfaces *every* moved node.
        match self.whole_graph_resolve(project, plan).await {
            Ok(()) => {}
            Err(err) if err.is_tool_spawn_failure() => return Err(err),
            // A non-spawn failure during the resolve (e.g. an unreadable lock between rounds): the
            // caller restores the journal, so no partial lock is kept.
            Err(_) => {
                for change in &plan.changes {
                    report.skipped.push(Skipped {
                        change: change.clone(),
                        reason: SkipReason::ResolverConflict,
                        offending: Some(change.package.clone()),
                    });
                }
                return Ok(report);
            }
        }

        let after = locked_versions(&read_lock(project)?);
        // The resolved graph, used to name the crate whose `=`-pin holds a short candidate back.
        let graph = self.cargo.metadata(&project.root).await.ok();
        let planned: BTreeSet<&str> = plan
            .changes
            .iter()
            .map(|change| change.package.name.as_str())
            .collect();

        // Each planned candidate either reached cooldown's target (its newest-within-window) —
        // reported applied — or fell short because a mutually-exclusive `=`-pin or single-major shared
        // transitive won — reported held, naming the blocker.
        for change in &plan.changes {
            if reached(&after, change) {
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

        // The hard requirement: no net version change to *any* crate may be omitted. A crate the plan
        // did not name that the whole-graph resolve moved (a transitive pushed backward for
        // consistency, or matured down by `fix`) is surfaced as its own collateral applied row — the
        // silent, unreported drift the earlier per-precise-pin design allowed.
        let mut collateral: Vec<Change> = before
            .iter()
            .filter(|((name, _), _)| !planned.contains(name.as_str()))
            .filter_map(|((name, _), from)| {
                let to = after.get(&(name.clone(), version::major_key(from).0))?;
                (version::compare(from, to).is_ne()).then(|| collateral_change(name, from, to))
            })
            .collect();
        collateral.sort_by(|a, b| a.package.name.cmp(&b.package.name));
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

    /// The collateral net version changes the diff surfaces for crates the plan did not name — the
    /// before/after pairs that differ, excluding the planned set. Mirrors `apply`'s collateral pass.
    fn collateral_changes(
        before: &BTreeMap<SlotKey, String>,
        after: &BTreeMap<SlotKey, String>,
        planned: &BTreeSet<&str>,
    ) -> Vec<Change> {
        let mut changes: Vec<Change> = before
            .iter()
            .filter(|((name, _), _)| !planned.contains(name.as_str()))
            .filter_map(|((name, _), from)| {
                let to = after.get(&(name.clone(), version::major_key(from).0))?;
                (version::compare(from, to).is_ne()).then(|| collateral_change(name, from, to))
            })
            .collect();
        changes.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        changes
    }

    fn planned_set<'a>(names: &'a [&'a str]) -> BTreeSet<&'a str> {
        names.iter().copied().collect()
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
        // distinct slots.
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
    fn reached_respects_direction_and_major_slot() {
        let after = locked_versions(&lock_with(&[("serde", "1.0.200"), ("syn", "2.0.50")]));
        // A forward candidate must land at/above its target in the target's major slot.
        assert!(reached(
            &after,
            &change("serde", "1.0.100", "1.0.200", false)
        ));
        assert!(!reached(
            &after,
            &change("serde", "1.0.100", "1.0.250", false)
        ));
        // A downgrade must land at/below its target.
        assert!(reached(&after, &change("syn", "2.0.60", "2.0.50", true)));
        assert!(!reached(&after, &change("syn", "2.0.60", "2.0.40", true)));
        // A candidate absent from the lock did not reach its target.
        assert!(!reached(&after, &change("tokio", "1.0.0", "1.5.0", false)));
    }

    #[test]
    fn collateral_change_surfaces_a_forced_non_candidate_downgrade() {
        // Raising `a` forces the shared transitive `shared` from 1.1.0 down to 1.0.0 as a consistency
        // move. `shared` is not planned, so the diff must surface it as its own collateral row — the
        // silent drift the earlier per-precise-pin design allowed.
        let before = locked_versions(&lock_with(&[("a", "1.0.0"), ("shared", "1.1.0")]));
        let after = locked_versions(&lock_with(&[("a", "2.0.0"), ("shared", "1.0.0")]));
        let collateral = collateral_changes(&before, &after, &planned_set(&["a"]));
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
    fn collateral_change_excludes_planned_and_unchanged_packages() {
        // `a` is planned (its own row, not collateral), `b` is unchanged (no row), `c` is an unplanned
        // forward move (a real collateral change). Only `c` is surfaced.
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
        let collateral = collateral_changes(&before, &after, &planned_set(&["a"]));
        assert_eq!(collateral.len(), 1);
        assert_eq!(collateral[0].package.name, "c");
        assert!(!collateral[0].downgrade);
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
