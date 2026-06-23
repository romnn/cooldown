//! The Python/uv [`Tool`]: detection, the resolved graph + per-file upload times from
//! `uv.lock`, `PyPI` as the publish-time fallback, native `[tool.uv]` cooldown config, and
//! `uv`-driven resolution/apply. The core owns the verdict; uv only resolves/applies a window.

use crate::artifact::published_at_for_artifacts;
use crate::lock::UvLock;
use crate::manifest;
use crate::native::parse_native;
use crate::pypi::{PYPI, PyPi};
use crate::uvcmd::Uv;
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{build_registry_releases, verify_current_report};
use cooldown_core::{
    ApplyReport, ArtifactScope, Capabilities, Change, DepScope, Dependency, FetchContext,
    MemberRef, NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality,
    ResolveInputs, ResolvedPolicy, Result, RewriteMode, SkipReason, Skipped, SyncReport, SyncScope,
    ToolId, ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

/// The [`ToolId`] for the Python/uv adapter (`"uv"`).
pub const UV_ID: ToolId = ToolId("uv");

/// The Python/uv implementation of the [`Tool`] port.
///
/// It detects `uv.lock` projects, reads the resolved graph and per-file upload
/// times from the lock (falling back to [`PyPi`] for the publish instant), parses
/// `[tool.uv]` cooldown config as a native policy layer, and drives the `uv` CLI
/// to re-resolve and apply a chosen window. The verdict itself is the core's;
/// uv only resolves and applies.
pub struct UvTool {
    pypi: PyPi,
    uv: Uv,
}

impl UvTool {
    /// Creates the adapter from a configured [`PyPi`] client.
    #[must_use]
    pub fn new(pypi: PyPi) -> Self {
        UvTool {
            pypi,
            uv: Uv::new(),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`PyPi`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        UvTool::new(PyPi::new(http))
    }
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// Builds the sorted, deduplicated [`Release`] list the core consumes.
///
/// Unparsable versions are dropped, the rest are sorted by [`version::compare`]
/// and deduplicated, then each is stamped with its update kind relative to
/// `current`, its quality, and an opaque [`ReleaseOrder`] token: the big-endian
/// index, so byte-lexicographic order matches PEP 440 order. The index is widened
/// with [`u32::try_from`] and saturated at [`u32::MAX`], which a real release count
/// can never reach.
#[must_use]
pub fn build_releases(
    current: &str,
    raw: Vec<RawRelease>,
    dep: &Dependency,
    fetch: &FetchContext<'_>,
) -> Vec<Release> {
    let raw: Vec<RawRelease> = raw
        .into_iter()
        .map(|mut release| {
            if matches!(fetch.artifacts, ArtifactScope::Environment) {
                release.published_at =
                    published_at_for_artifacts(&release.artifacts, &dep.artifacts);
            }
            release
        })
        .collect();
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

fn read_lock(project: &Project) -> Result<UvLock> {
    let content = std::fs::read_to_string(project.root.join("uv.lock"))?;
    UvLock::parse(&content)
}

#[async_trait]
impl ToolRead for UvTool {
    fn id(&self) -> ToolId {
        UV_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: true,
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        // Each `uv.lock` marks an independent project. A uv *workspace* keeps a single lock at its
        // root and its members carry only a `pyproject.toml` (no nested lock), so a `uv.lock` found
        // below another is never a workspace member — it is a separate project that resolves on its
        // own and must be synced/checked in its own right. Hence `workspace_root: false`: nested
        // locks are not collapsed into the topmost one.
        ProjectMarker {
            lockfile: "uv.lock",
            manifest: "pyproject.toml",
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let lock = read_lock(project)?;
        let direct: std::collections::HashSet<String> = lock.direct_names().into_iter().collect();
        let floors = lock.graph_floors();
        let ceilings = lock.graph_ceilings();
        let exact_pins = crate::native::exact_pinned_names(&project.manifest);
        // A uv project is a single package, so it is the source for every dependency it declares.
        // The lock's root package carries the project's package name.
        let project_member: Vec<MemberRef> = lock
            .packages
            .iter()
            .find(|pkg| {
                pkg.source
                    .as_ref()
                    .is_some_and(crate::lock::Source::is_project_root)
            })
            .map(|pkg| {
                vec![MemberRef {
                    name: pkg.name.clone(),
                    path: ".".to_string(),
                }]
            })
            .unwrap_or_default();

        let mut deps = Vec::new();
        for pkg in &lock.packages {
            let Some(source) = &pkg.source else { continue };
            if source.is_root() || !source.is_registry() {
                continue; // skip the root project and non-registry (path/git) packages
            }
            let Some(version) = &pkg.version else {
                continue;
            };
            let is_direct = direct.contains(&pkg.name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            // The project's own exact pin surfaces as `pinned` (held, with a repin target shown); the
            // ceiling is reserved for caps imposed by *other* requirers, so it is only set when the
            // dependency is not already pinned (mirroring the cargo adapter).
            let pinned = exact_pins.contains(&pkg.name);
            // A requirer's `==` pin caps this package only when it is *active*: some pinned version
            // must equal the version uv resolved here. A marker-gated or otherwise inactive pin
            // resolves to a different version and imposes no real bound, so it is ignored. The
            // resolved version is recorded as the ceiling so it is the canonical form that matches a
            // fetched release (the raw specifier may not be PEP 440-normalized).
            let graph_ceiling = (!pinned)
                .then(|| ceilings.get(&pkg.name))
                .flatten()
                .filter(|pins| {
                    pins.iter()
                        .any(|pin| crate::version::compare(pin, version).is_eq())
                })
                .map(|_| Version::new(version.clone()));
            deps.push(Dependency {
                package: PackageId::new(UV_ID, pkg.name.clone(), Some(PYPI.to_string())),
                current: Version::new(version.clone()),
                current_quality: classify_quality(version),
                direct: is_direct,
                artifacts: pkg.artifact_ids(),
                graph_floor: floors.get(&pkg.name).map(|v| Version::new(v.clone())),
                graph_ceiling,
                members: if is_direct {
                    project_member.clone()
                } else {
                    Vec::new()
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
        match self
            .uv
            .verify_check(&project.root, project.exclude_newer.as_deref())
            .await
        {
            Ok(ok) => Ok(verify_current_report(
                ok,
                "uv.lock is current",
                "uv.lock is stale; run `uv lock`",
            )),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl ReleaseFetcher for UvTool {
    async fn releases(
        &self,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        _candidates: cooldown_core::CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.pypi.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw, dep, fetch))
    }

    fn releases_are_project_scoped(&self) -> bool {
        // uv is artifact-granular: `releases`/`locked_release` derive publish instants from the
        // project's own locked artifacts (`dep.artifacts`, and this project's `uv.lock`), so the
        // answer differs per project. The cache must key by project, not share across uv projects.
        true
    }

    async fn locked_release(&self, dep: &Dependency, fetch: &FetchContext<'_>) -> Result<Release> {
        // Prefer the lock's recorded per-file upload time; fall back to PyPI.
        let from_lock = read_lock(fetch.project).ok().and_then(|lock| {
            lock.find(&dep.package.name, dep.current.as_str())
                .and_then(|package| {
                    let selected = match fetch.artifacts {
                        ArtifactScope::Environment => dep.artifacts.as_slice(),
                        ArtifactScope::All => &[],
                    };
                    package.published_at_for_artifacts(selected)
                })
        });
        let time = match from_lock {
            Some(t) => Some(t),
            None => {
                self.pypi
                    .published_at(
                        &dep.package,
                        &dep.current,
                        match fetch.artifacts {
                            ArtifactScope::Environment => &dep.artifacts,
                            ArtifactScope::All => &[],
                        },
                    )
                    .await?
            }
        };
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

/// The resolved registry-package versions of a lock, keyed by package name — the snapshot `apply`
/// diffs before/after the whole-graph re-resolve to report *every* net version change (the upgrades,
/// the downgrades the resolve forced for consistency, and the candidates left held below their
/// newest-within-window). Only registry packages carry a comparable version; the root and path/git
/// sources are skipped.
fn locked_versions(lock: &UvLock) -> std::collections::HashMap<String, String> {
    lock.packages
        .iter()
        .filter(|pkg| {
            pkg.source
                .as_ref()
                .is_some_and(crate::lock::Source::is_registry)
        })
        .filter_map(|pkg| {
            pkg.version
                .clone()
                .map(|version| (pkg.name.clone(), version))
        })
        .collect()
}

/// The package whose requirement structurally holds `held` out of the graph at `target`, checked in
/// **both** directions so a blocker is named whichever side carries the constraint:
///
/// 1. A *requirer* of `held`: a committed package whose `requires-dist` upper bound on `held` excludes
///    `target` (e.g. `huggingface-hub` at 1.18.0 requiring `typer<0.26.0` keeps `typer` below 0.26.x).
/// 2. A *sibling* `held` itself constrains: `held`'s own committed `requires-dist` carries an upper
///    bound on some package whose currently-committed version already sits at or above that bound — so
///    raising `held` to `target` (which carries the same structural cap) would conflict with that
///    sibling. This names the counterpart of the mutual exclusion when no requirer of `held` exists
///    (e.g. the `held` package is the one carrying the `<` requirement, not the one being capped).
///
/// Only the upper bound of a specifier is parsed — the form that excludes a *newer* version; an
/// unbounded or lower-only requirement never holds a candidate down. Skips `held` itself and is
/// order-stable. Returns `None` when neither direction yields a name, so the caller falls back to the
/// generic "the resolver rejected this change".
fn blocking_requirer(lock: &UvLock, held: &str, target: &str) -> Option<String> {
    if let Some(requirer) = requirer_capping(lock, held, target) {
        return Some(requirer);
    }
    if let Some(sibling) = sibling_held_caps(lock, held) {
        return Some(sibling);
    }
    unique_edge_requirer(lock, held)
}

/// Last-resort best-effort: the single package whose *resolved* dependency edge reaches `held`. Modern
/// `uv.lock` files often omit per-package `requires-dist`, so neither version-bound direction can name
/// the blocker; the resolved edges remain, and a transitive held below its newest is structurally held
/// by the package that pulls it. Names that package only when it is *unique* — multiple requirers make
/// the blame ambiguous, so the caller falls back to the generic message rather than guess. Skips
/// `held` itself.
fn unique_edge_requirer(lock: &UvLock, held: &str) -> Option<String> {
    let mut requirers: Vec<&str> = lock
        .packages
        .iter()
        .filter(|pkg| pkg.name != held)
        .filter(|pkg| pkg.all_direct_dep_names().any(|name| name == held))
        .map(|pkg| pkg.name.as_str())
        .collect();
    requirers.sort_unstable();
    requirers.dedup();
    match requirers.as_slice() {
        [only] => Some((*only).to_string()),
        _ => None,
    }
}

/// Direction 1: a committed package (other than `held`) whose `requires-dist` upper bound on `held`
/// excludes `target`. Order-stable across candidate requirers.
fn requirer_capping(lock: &UvLock, held: &str, target: &str) -> Option<String> {
    let mut requirers: Vec<&str> = lock
        .packages
        .iter()
        .filter(|pkg| pkg.name != held)
        .filter(|pkg| {
            pkg.metadata.as_ref().is_some_and(|metadata| {
                metadata.requires_dist.iter().any(|req| {
                    req.name == held
                        && req
                            .specifier
                            .as_deref()
                            .and_then(specifier_upper_bound)
                            .is_some_and(|bound| version::compare(target, bound).is_ge())
                })
            })
        })
        .map(|pkg| pkg.name.as_str())
        .collect();
    requirers.sort_unstable();
    requirers.into_iter().next().map(str::to_string)
}

/// Direction 2: a sibling that `held`'s own committed requirement caps below the sibling's
/// currently-committed version. When `held` is the package carrying the `<` bound (rather than the one
/// being capped), the structural counterpart is the sibling whose committed version that bound
/// excludes — naming it explains why `held` cannot move up without regressing the sibling. Order-stable
/// across siblings.
fn sibling_held_caps(lock: &UvLock, held: &str) -> Option<String> {
    let committed = |name: &str| -> Option<&str> {
        lock.packages
            .iter()
            .find(|pkg| pkg.name == name)
            .and_then(|pkg| pkg.version.as_deref())
    };
    let held_pkg = lock.packages.iter().find(|pkg| pkg.name == held)?;
    let metadata = held_pkg.metadata.as_ref()?;
    let mut siblings: Vec<&str> = metadata
        .requires_dist
        .iter()
        .filter(|req| req.name != held)
        .filter(|req| {
            let Some(bound) = req.specifier.as_deref().and_then(specifier_upper_bound) else {
                return false;
            };
            committed(&req.name).is_some_and(|version| version::compare(version, bound).is_ge())
        })
        .map(|req| req.name.as_str())
        .collect();
    siblings.sort_unstable();
    siblings.into_iter().next().map(str::to_string)
}

/// The exclusive/inclusive upper-bound version of a PEP 440 specifier, if it carries one (`<X`,
/// `<=X`, or a compound `>=A,<X`). A target at or above this bound is excluded by the requirement, so
/// the requirer is holding the dependency below it. Returns `None` for unbounded or lower-only
/// specifiers, which never cap a newer target.
fn specifier_upper_bound(specifier: &str) -> Option<&str> {
    specifier
        .split(',')
        .filter_map(|clause| {
            let clause = clause.trim();
            clause
                .strip_prefix("<=")
                .or_else(|| clause.strip_prefix('<'))
                .map(str::trim)
        })
        .filter(|bound| !bound.is_empty() && !bound.contains('*'))
        .min_by(|a, b| version::compare(a, b))
}

/// A net version change `apply` derived from the before/after lock diff for a package the plan did not
/// itself name — collateral movement the whole-graph re-resolve forced. Reported so no package's
/// version change is ever silent: a forced downgrade of a non-candidate (e.g. `typer` regressing
/// because `huggingface-hub` rose) surfaces as its own report row.
fn collateral_change(name: &str, from: &str, to: &str) -> Change {
    let from_version = Version::new(from.to_string());
    let to_version = Version::new(to.to_string());
    let downgrade = version::compare(to, from).is_lt();
    Change {
        package: PackageId::new(UV_ID, name.to_string(), Some(PYPI.to_string())),
        from: from_version,
        to: to_version,
        // A collateral move is always transitive consistency churn, not a directly-declared bump, and
        // its update kind is informational only; `Minor` is the neutral label the renderer shows.
        kind: cooldown_core::UpdateKind::Minor,
        downgrade,
        direct: false,
        members: Vec::new(),
    }
}

impl UvTool {
    /// Re-resolve the **whole** graph once under cooldown's window, honoring [`RewriteMode`] for the
    /// planned candidates and any per-package ceilings cooldown's verdict imposes.
    ///
    /// `upgrade` selects `uv lock --upgrade` (forward, maximal-within-window) versus a plain re-lock
    /// (minimal, matures only too-fresh pins down — the `fix`/reconcile form). Widening runs only for
    /// the planned candidates whose declared `pyproject` requirement would otherwise cap them below
    /// their target: `Always` widens every candidate up front; `Auto` widens only those the resolve
    /// left short of their target because of their *own* declared cap (a candidate held by *another*
    /// package's requirement is a real conflict the full-lock diff then reports, not a missing widen).
    ///
    /// After the global-window resolve, any planned candidate that landed *newer* than cooldown's
    /// target (because that package carries a stricter-than-global per-package window, a floor, or an
    /// exempt freeze) is re-capped to its target via uv's `--upgrade-package <name><=<target>` and the
    /// graph re-resolved, so a per-package window is enforced natively without pinning the rest of the
    /// graph. The uniform-window case adds no ceilings and is a single whole-graph resolve.
    async fn whole_graph_resolve(
        &self,
        project: &Project,
        plan: &Plan,
        upgrade: bool,
    ) -> Result<()> {
        if matches!(plan.rewrite, RewriteMode::Always) {
            for change in &plan.changes {
                manifest::widen_constraint(
                    &project.manifest,
                    &change.package.name,
                    change.to.as_str(),
                )?;
            }
        }
        self.lock_once(project, upgrade, &[]).await?;

        if upgrade && matches!(plan.rewrite, RewriteMode::Auto) {
            // Widen only the candidates uv could not raise to their target because their own declared
            // requirement caps them, then re-resolve. A candidate still short after a no-op widen round
            // is blocked by another package (a real conflict), so widening stops and the diff reports it.
            for _ in 0..plan.changes.len() {
                let after = locked_versions(&read_lock(project)?);
                let mut widened_any = false;
                for change in &plan.changes {
                    let reached = after.get(&change.package.name).is_some_and(|current| {
                        version::compare(current, change.to.as_str()).is_ge()
                    });
                    if reached {
                        continue;
                    }
                    if manifest::widen_constraint(
                        &project.manifest,
                        &change.package.name,
                        change.to.as_str(),
                    )? {
                        widened_any = true;
                    }
                }
                if !widened_any {
                    break;
                }
                self.lock_once(project, upgrade, &[]).await?;
            }
        }

        // Per-package window enforcement: a candidate the global-window resolve placed *above*
        // cooldown's target carries a stricter-than-global window (a longer per-package age, a floor,
        // or an exempt freeze that cooldown already folded into the target). Cap exactly those packages
        // at their target and re-resolve; the uniform case finds none and skips this entirely.
        let after = locked_versions(&read_lock(project)?);
        let ceilings: Vec<(String, String)> = plan
            .changes
            .iter()
            .filter(|change| {
                after
                    .get(&change.package.name)
                    .is_some_and(|locked| version::compare(locked, change.to.as_str()).is_gt())
            })
            .map(|change| (change.package.name.clone(), change.to.as_str().to_string()))
            .collect();
        if !ceilings.is_empty() {
            self.lock_once(project, upgrade, &ceilings).await?;
        }
        Ok(())
    }

    async fn lock_once(
        &self,
        project: &Project,
        upgrade: bool,
        ceilings: &[(String, String)],
    ) -> Result<()> {
        self.uv
            .lock_resolve(
                &project.root,
                upgrade,
                ceilings,
                project.exclude_newer.as_deref(),
            )
            .await
    }
}

#[async_trait]
impl ToolWrite for UvTool {
    fn resolve_inputs(&self) -> ResolveInputs {
        // `uv lock` builds local/workspace-member metadata via the PEP 517 backend for a `dynamic`
        // version or `readme`/`license = {file = ...}`, which reads `.py` source (e.g. `_version.py`).
        // The throwaway probe copy must include it; a static-version project ignores the extra files.
        ResolveInputs {
            source_extensions: &["py"],
            ..ResolveInputs::DEFAULT
        }
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        // Capture the lock and the manifest: `apply` re-locks (uv.lock) and, when the target falls
        // outside the declared requirement, rewrites the constraint (pyproject.toml). Capturing the
        // manifest unconditionally is harmless — restore runs only on rollback.
        Ok(ProjectMutationJournal {
            files: vec![
                ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("uv.lock"))?,
                ProjectMutationJournal::capture_file(
                    &project.root,
                    Utf8Path::new("pyproject.toml"),
                )?,
            ],
        })
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

        // The pre-apply lock, taken from the journal (`mutation_journal` captured `uv.lock` before the
        // re-resolve). The whole-graph resolve emits one consistent lock; the report is the diff of this
        // snapshot against the result, so *every* net version change is surfaced. A missing/unparsable
        // snapshot leaves `before` empty, so a package that moved is still reported (never silent).
        let before = journal
            .files
            .iter()
            .find(|file| file.path == Utf8Path::new("uv.lock"))
            .and_then(|file| file.contents.as_deref())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|content| UvLock::parse(content).ok())
            .map(|lock| locked_versions(&lock))
            .unwrap_or_default();

        // The whole graph is re-resolved in one pass under cooldown's window: `--upgrade` for forward
        // moves (maximal-within-window), a plain re-lock for an all-downgrade plan (`fix`/reconcile —
        // mature only the too-fresh pins). uv settles every conflict itself, so the result is the unique
        // fixed point under the window — no per-package pins for the bulk of the graph, no oscillation,
        // and no package left to drift unreported because *every* package is re-resolved under the cutoff.
        let upgrade = !plan.changes.iter().all(|change| change.downgrade);
        match self.whole_graph_resolve(project, plan, upgrade).await {
            Ok(()) => {}
            Err(err) if err.is_tool_spawn_failure() => return Err(err),
            // The whole resolve is unsatisfiable (no consistent lock under the window). Propagate so
            // the caller's `apply_resilient` can isolate the offending candidate(s) and apply the rest,
            // instead of holding every candidate. The caller restores the journal, so no partial lock
            // is kept.
            Err(err) => return Err(err),
        }

        let after_lock = read_lock(project)?;
        let after = locked_versions(&after_lock);
        let planned: std::collections::HashSet<&str> = plan
            .changes
            .iter()
            .map(|change| change.package.name.as_str())
            .collect();

        // Each planned candidate either reached cooldown's target (its newest-within-window) — reported
        // applied — or fell short because a mutually-exclusive requirement won — reported held, naming
        // the blocker. "Reached" respects the move's direction: a forward candidate must land at or
        // above its target, a downgrade at or below it.
        for change in &plan.changes {
            let landed = after.get(change.package.name.as_str());
            let reached = landed.is_some_and(|version| {
                let ordering = version::compare(version, change.to.as_str());
                if change.downgrade {
                    ordering.is_le()
                } else {
                    ordering.is_ge()
                }
            });
            if reached {
                report.applied.push(change.clone());
            } else {
                // uv could not place this candidate at its target without breaking the lock — another
                // requirement won. Name the package whose upper-bound requirement structurally holds it
                // below the target so the report says "held: conflicts with <pkg>"; absent a known
                // blocker it falls back to the candidate itself (the generic "resolver rejected" form).
                let offender =
                    blocking_requirer(&after_lock, &change.package.name, change.to.as_str())
                        .unwrap_or_else(|| change.package.name.clone());
                report.skipped.push(Skipped {
                    change: change.clone(),
                    reason: SkipReason::ResolverConflict,
                    offending: Some(PackageId::new(UV_ID, offender, Some(PYPI.to_string()))),
                });
            }
        }

        // The hard requirement: no net version change to *any* package may be omitted. A package the
        // plan did not name that the whole-graph resolve moved (a transitive forced backward to keep the
        // lock consistent, or a transitive matured down by `fix`) is surfaced as its own collateral
        // applied row — the silent, unreported drift that earlier per-candidate designs allowed.
        let mut collateral: Vec<Change> = before
            .iter()
            .filter(|(name, _)| !planned.contains(name.as_str()))
            .filter_map(|(name, from)| {
                let to = after.get(name)?;
                (version::compare(from, to).is_ne()).then(|| collateral_change(name, from, to))
            })
            .collect();
        collateral.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        report.applied.extend(collateral);
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.uv
            .sync(&project.root, project.exclude_newer.as_deref())
            .await
    }

    // uv's native cooldown config is repo-scoped (`SyncScope::Repo`), not per-project: the per-project
    // `[tool.uv] exclude-newer` in a `pyproject.toml` is inert to uv and a per-project `uv.toml` would
    // shadow that project's `[tool.uv]` sources/overrides. So `write_native` stays the default
    // `Unsupported` (cooldown drives the window directly via `--exclude-newer` / `UV_EXCLUDE_NEWER` on
    // every invocation) and the sync instead writes a single repo-root `uv.toml` via
    // `write_repo_native`. The native *reader* still honors a window a project declares.

    fn sync_scope(&self) -> SyncScope {
        SyncScope::Repo
    }

    async fn write_repo_native(
        &self,
        repo_root: &Utf8Path,
        policy: &ResolvedPolicy,
        dry_run: bool,
    ) -> Result<SyncReport> {
        let path = repo_root.join("uv.toml");
        let Some(value) = policy
            .default_window
            .as_ref()
            .and_then(cooldown_core::window_exclude_newer)
        else {
            // A `Latest`/zero window excludes nothing, so there is no native cutoff to write.
            return Ok(SyncReport::Unchanged { path });
        };
        // A `uv.toml` is a flat config file, so the key is the top-level `exclude-newer` — not the
        // `[tool.uv]`-nested form a `pyproject.toml` uses.
        let written =
            cooldown_toml_util::set_toml_string(&path, &["exclude-newer"], &value, dry_run)?;
        if written {
            Ok(SyncReport::Written { path })
        } else {
            Ok(SyncReport::Unchanged { path })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use cooldown_adapter_util::skipped_on_apply_error;
    use cooldown_core::{ArtifactId, Change, CoreError, FetchContext, RawArtifact, RawRelease};
    use indoc::indoc;
    use jiff::Timestamp;

    fn lock_with(packages: &[(&str, &str)]) -> UvLock {
        use std::fmt::Write as _;
        let mut content = String::from("version = 1\nrevision = 3\n");
        for (name, version) in packages {
            write!(
                content,
                "\n[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = {{ registry = \"https://pypi.org/simple\" }}\n"
            )
            .expect("writing to a String never fails");
        }
        UvLock::parse(&content).expect("lock parses")
    }

    /// The collateral net version changes the diff surfaces for packages the plan did not name — the
    /// before/after pairs that differ, excluding the planned set. Mirrors `apply`'s collateral pass.
    fn collateral_changes(
        before: &std::collections::HashMap<String, String>,
        after: &std::collections::HashMap<String, String>,
        planned: &std::collections::HashSet<&str>,
    ) -> Vec<Change> {
        let mut changes: Vec<Change> = before
            .iter()
            .filter(|(name, _)| !planned.contains(name.as_str()))
            .filter_map(|(name, from)| {
                let to = after.get(name)?;
                (version::compare(from, to).is_ne()).then(|| collateral_change(name, from, to))
            })
            .collect();
        changes.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        changes
    }

    #[test]
    fn collateral_change_surfaces_a_forced_non_candidate_downgrade() {
        // Raising `huggingface-hub` forces `typer` from 0.26.7 down to 0.25.1 as a consistency move.
        // `typer` is not planned, so the diff must surface it as its own collateral row — the silent,
        // unreported drift earlier per-candidate designs allowed.
        let before = locked_versions(&lock_with(&[
            ("huggingface-hub", "1.16.1"),
            ("typer", "0.26.7"),
        ]));
        let after = locked_versions(&lock_with(&[
            ("huggingface-hub", "1.18.0"),
            ("typer", "0.25.1"),
        ]));
        let collateral = collateral_changes(&before, &after, &planned_set(&["huggingface-hub"]));
        assert_eq!(collateral.len(), 1);
        let typer = &collateral[0];
        assert_eq!(typer.package.name, "typer");
        assert_eq!(typer.from.as_str(), "0.26.7");
        assert_eq!(typer.to.as_str(), "0.25.1");
        assert!(
            typer.downgrade,
            "a forced regression is reported as a downgrade"
        );
    }

    #[test]
    fn collateral_change_excludes_planned_and_unchanged_packages() {
        // `a` is planned (its own row, not collateral), `b` is unchanged (no row at all), and `c` is an
        // unplanned forward move (a real collateral change). Only `c` is surfaced.
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

    fn planned_set<'a>(names: &'a [&'a str]) -> std::collections::HashSet<&'a str> {
        names.iter().copied().collect()
    }

    #[test]
    fn specifier_upper_bound_reads_only_the_upper_clause() {
        assert_eq!(specifier_upper_bound("<0.26.0"), Some("0.26.0"));
        assert_eq!(specifier_upper_bound("<=1.4"), Some("1.4"));
        assert_eq!(specifier_upper_bound(">=0.20.0,<0.26.0"), Some("0.26.0"));
        // Lower-only and unbounded specifiers never cap a newer target.
        assert_eq!(specifier_upper_bound(">=1.1.5"), None);
        assert_eq!(specifier_upper_bound("==6.33.5"), None);
        assert_eq!(specifier_upper_bound("<*"), None);
    }

    #[test]
    fn blocking_requirer_names_a_structural_upper_bound_holder() {
        // `huggingface-hub` (committed at 1.18.0) requires `typer<0.26.0`, which holds the `typer`
        // candidate at 0.25.1 below its 0.26.7 target. The requirement names the structural blocker so
        // the held skip can say "held: conflicts with huggingface-hub".
        let lock = UvLock::parse(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "huggingface-hub"
            version = "1.18.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "typer" }]

            [package.metadata]
            requires-dist = [{ name = "typer", specifier = ">=0.20.0,<0.26.0" }]

            [[package]]
            name = "typer"
            version = "0.25.1"
            source = { registry = "https://pypi.org/simple" }
        "#})
        .expect("lock parses");
        assert_eq!(
            blocking_requirer(&lock, "typer", "0.26.7"),
            Some("huggingface-hub".to_string())
        );
        // When the target is *within* the requirement's bound, the upper-bound direction names no
        // blocker (the edge fallback is a last resort the full `blocking_requirer` only applies to a
        // genuinely-held candidate, so the bound direction is tested directly here).
        assert_eq!(requirer_capping(&lock, "typer", "0.25.5"), None);
    }

    #[test]
    fn blocking_requirer_names_the_sibling_a_held_package_caps() {
        // Here `huggingface-hub` is the *held* candidate carrying the `<` requirement (`typer<0.26.0`),
        // and `typer` is committed at 0.25.1 — within the bound, so no requirer caps `huggingface-hub`.
        // Direction 2 names `typer` as the structural counterpart only when its committed version
        // already sits at/above the bound `huggingface-hub` declares.
        let lock = UvLock::parse(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "huggingface-hub"
            version = "1.18.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "typer" }]

            [package.metadata]
            requires-dist = [{ name = "typer", specifier = ">=0.20.0,<0.26.0" }]

            [[package]]
            name = "typer"
            version = "0.26.7"
            source = { registry = "https://pypi.org/simple" }
        "#})
        .expect("lock parses");
        // No package requires `huggingface-hub` with a cap, but its own requirement caps `typer` below
        // the committed 0.26.7, so the sibling `typer` is named as the structural blocker.
        assert_eq!(
            blocking_requirer(&lock, "huggingface-hub", "1.19.0"),
            Some("typer".to_string())
        );
    }

    #[test]
    fn blocking_requirer_falls_back_to_a_unique_resolved_edge() {
        // A real `uv.lock` often records only resolved *edges* (no per-package `requires-dist`), so
        // neither version-bound direction can name the blocker. The unique package whose edge reaches
        // the held transitive is named as the best-effort structural blocker.
        let lock = UvLock::parse(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "huggingface-hub"
            version = "1.18.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "typer" }]

            [[package]]
            name = "typer"
            version = "0.25.1"
            source = { registry = "https://pypi.org/simple" }
        "#})
        .expect("lock parses");
        assert_eq!(
            blocking_requirer(&lock, "typer", "0.26.7"),
            Some("huggingface-hub".to_string())
        );
    }

    #[test]
    fn blocking_requirer_is_generic_when_multiple_edges_make_blame_ambiguous() {
        // Two packages pull the held transitive via resolved edges and neither carries a nameable
        // bound: blame is ambiguous, so no blocker is named and the caller keeps the generic message.
        let lock = UvLock::parse(indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "alpha"
            version = "1.0.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "typer" }]

            [[package]]
            name = "beta"
            version = "1.0.0"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "typer" }]

            [[package]]
            name = "typer"
            version = "0.25.1"
            source = { registry = "https://pypi.org/simple" }
        "#})
        .expect("lock parses");
        assert_eq!(blocking_requirer(&lock, "typer", "0.26.7"), None);
    }

    #[test]
    fn apply_spawn_failure_is_not_downgraded_to_skip() {
        let change = Change {
            package: PackageId::new(UV_ID, "requests", Some(PYPI.to_string())),
            from: Version::new("2.34.1"),
            to: Version::new("2.34.2"),
            kind: cooldown_core::UpdateKind::Patch,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        let err = CoreError::ToolSpawn {
            tool: "uv".into(),
            detail: "spawn failed".into(),
        };

        let result = skipped_on_apply_error(&change, err);
        assert!(matches!(result, Err(CoreError::ToolSpawn { .. })));
    }

    #[test]
    fn build_releases_respects_environment_artifact_scope() {
        let project = Project {
            root: Utf8PathBuf::from("."),
            kind: UV_ID,
            manifest: Utf8PathBuf::from("pyproject.toml"),
            exclude_newer: None,
        };
        let dep = Dependency {
            package: PackageId::new(UV_ID, "requests", Some(PYPI.to_string())),
            current: Version::new("2.32.0"),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: vec![ArtifactId("wheel:py3-none-any".into())],
            graph_floor: None,
            graph_ceiling: None,
            members: Vec::new(),
            pinned: false,
        };
        let raw = vec![RawRelease {
            version: Version::new("2.32.1"),
            published_at: Some("2026-06-05T00:00:00Z".parse::<Timestamp>().unwrap()),
            yanked: false,
            artifacts: vec![
                RawArtifact {
                    id: ArtifactId("wheel:py3-none-any".into()),
                    published_at: Some("2026-06-01T00:00:00Z".parse::<Timestamp>().unwrap()),
                    markers: Vec::new(),
                },
                RawArtifact {
                    id: ArtifactId("sdist".into()),
                    published_at: Some("2026-06-05T00:00:00Z".parse::<Timestamp>().unwrap()),
                    markers: Vec::new(),
                },
            ],
        }];

        let env_fetch = FetchContext {
            project: &project,
            artifacts: ArtifactScope::Environment,
        };
        let all_fetch = FetchContext {
            project: &project,
            artifacts: ArtifactScope::All,
        };

        let env_releases = build_releases(dep.current.as_str(), raw.clone(), &dep, &env_fetch);
        let all_releases = build_releases(dep.current.as_str(), raw, &dep, &all_fetch);

        assert_eq!(
            env_releases[0].published_at.unwrap().to_string(),
            "2026-06-01T00:00:00Z"
        );
        assert_eq!(
            all_releases[0].published_at.unwrap().to_string(),
            "2026-06-05T00:00:00Z"
        );
    }

    #[tokio::test]
    async fn dependencies_attribute_only_direct_declarations_to_project_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        let lock = indoc! {r#"
            version = 1
            revision = 3

            [[package]]
            name = "demo"
            version = "0.1.0"
            source = { virtual = "." }
            dependencies = [{ name = "requests" }]

            [[package]]
            name = "requests"
            version = "2.34.2"
            source = { registry = "https://pypi.org/simple" }
            dependencies = [{ name = "idna" }]

            [[package]]
            name = "idna"
            version = "3.10"
            source = { registry = "https://pypi.org/simple" }
        "#};
        std::fs::write(root.join("uv.lock"), lock).expect("write lock");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let tool = UvTool::from_http(
            cooldown_registry::SharedHttp::new(
                cache_dir.path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        let project = Project {
            root: root.clone(),
            kind: UV_ID,
            manifest,
            exclude_newer: None,
        };

        let graph = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("deps");
        let direct = graph
            .iter()
            .find(|dep| dep.package.name == "requests")
            .expect("direct dep");
        assert_eq!(
            direct
                .members
                .iter()
                .map(|member| (member.name.as_str(), member.path.as_str()))
                .collect::<Vec<_>>(),
            vec![("demo", ".")]
        );

        let transitive = graph
            .iter()
            .find(|dep| dep.package.name == "idna")
            .expect("transitive dep");
        assert!(!transitive.direct);
        assert!(
            transitive.members.is_empty(),
            "transitive dependencies are not declared by the project member"
        );
    }

    #[tokio::test]
    async fn mutation_journal_restore_removes_lock_created_after_capture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let manifest = root.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let eco = UvTool::from_http(
            cooldown_registry::SharedHttp::new(
                cache_dir.path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        let project = Project {
            root: root.clone(),
            kind: UV_ID,
            manifest,
            exclude_newer: None,
        };

        let journal = eco
            .mutation_journal(&project, &Plan::default())
            .await
            .expect("journal");
        let lock = root.join("uv.lock");
        std::fs::write(&lock, "generated").expect("write lock");

        journal.restore(&project.root).expect("restore");
        assert!(!lock.exists());
    }

    fn uv_tool() -> UvTool {
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let tool = UvTool::from_http(
            cooldown_registry::SharedHttp::new(
                cache_dir.path(),
                cooldown_registry::HttpOptions::default(),
            )
            .expect("http"),
        );
        // Keep the cache dir alive for the duration of the tool by leaking it: tests are short-lived
        // and this avoids a dangling temp path while the tool holds the HTTP client.
        std::mem::forget(cache_dir);
        tool
    }

    #[tokio::test]
    async fn write_repo_native_writes_flat_exclude_newer_once_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let tool = uv_tool();
        assert_eq!(tool.sync_scope(), SyncScope::Repo);

        let policy = ResolvedPolicy {
            default_window: Some(cooldown_core::WindowSpec::MinAge(
                jiff::SignedDuration::from_hours(24 * 14),
            )),
            exempt_packages: Vec::new(),
        };
        let uv_toml = root.join("uv.toml");

        let first = tool
            .write_repo_native(&root, &policy, false)
            .await
            .expect("write");
        assert!(matches!(first, SyncReport::Written { .. }));
        // The flat top-level key is written into the repo-root uv.toml, not the [tool.uv] nested form.
        let written = std::fs::read_to_string(&uv_toml).expect("read uv.toml");
        assert!(
            written.contains("exclude-newer = \"14 days\""),
            "unexpected uv.toml contents: {written}"
        );
        assert!(!written.contains("[tool.uv]"));

        // A second run with the same policy is a no-op.
        let second = tool
            .write_repo_native(&root, &policy, false)
            .await
            .expect("write");
        assert!(matches!(second, SyncReport::Unchanged { .. }));
    }

    #[tokio::test]
    async fn write_repo_native_reports_unchanged_for_latest_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let tool = uv_tool();
        let policy = ResolvedPolicy {
            default_window: Some(cooldown_core::WindowSpec::Latest),
            exempt_packages: Vec::new(),
        };

        let report = tool
            .write_repo_native(&root, &policy, false)
            .await
            .expect("write");
        assert!(matches!(report, SyncReport::Unchanged { .. }));
        // A `Latest` window excludes nothing, so no uv.toml is created.
        assert!(!root.join("uv.toml").exists());
    }
}
