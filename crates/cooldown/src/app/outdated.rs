//! `outdated` — what could update, split into "adoptable now" vs "in cooldown". Reasons over the
//! candidate set; informational, so per-dep failures never change the exit code.

use super::change_key::{ChangeTargetKey, change_target_key, change_target_key_parts};
use super::{
    Exit, LockReportAction, RunOpts, Workspace, age_days, diag_from_error, lock_report_outcome,
    render_window,
};
use super::{LatestInfo, OutdatedItem, OutdatedStatus, OutdatedSummary, UpgradeItem, Window};
use cooldown_core::{
    Change, DepScope, Dependency, Diagnostic, LockVerifyReport, PackageId, Release, ResolveKind,
    ResolveQuery, UpdateKind, Version, evaluate, resolve,
};
use std::collections::{HashMap, HashSet};

/// The result of `outdated`: every reported dependency split by status, plus diagnostics.
///
/// `outdated` is informational, so per-dependency failures become `Error` items rather than
/// changing the exit code; [`exit`](Self::exit) is therefore always [`Exit::Ok`].
pub struct OutdatedOutcome {
    /// Per-status counts across all reported items.
    pub summary: OutdatedSummary,
    /// One entry per in-scope dependency (including any that failed to evaluate).
    pub items: Vec<OutdatedItem>,
    /// Non-fatal diagnostics not attributable to a single dependency.
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors (e.g. a dependency graph that could not be enumerated).
    pub errors: Vec<Diagnostic>,
    /// Always [`Exit::Ok`]: this command never gates.
    pub exit: Exit,
}

struct OutdatedRunner<'a> {
    ws: &'a Workspace,
    opts: &'a RunOpts,
    scope: DepScope,
    items: Vec<OutdatedItem>,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
}

impl Workspace {
    /// Report what could update, split into "adoptable now" vs "in cooldown".
    ///
    /// Scopes to direct deps unless `--transitive` is set, and to packages matching `--package`.
    /// Surfaces a yanked locked version as a warning.
    pub async fn outdated(&self, opts: &RunOpts) -> OutdatedOutcome {
        OutdatedRunner::new(self, opts).run().await
    }
}

impl<'a> OutdatedRunner<'a> {
    fn new(ws: &'a Workspace, opts: &'a RunOpts) -> Self {
        let scope = if opts.transitive {
            DepScope::Graph
        } else {
            DepScope::Direct
        };
        OutdatedRunner {
            ws,
            opts,
            scope,
            items: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    async fn run(mut self) -> OutdatedOutcome {
        for pctx in self.ws.scoped_projects(self.opts) {
            self.run_project(pctx).await;
        }

        // Releases are fetched concurrently (`buffer_unordered`), so the items arrive in a
        // non-deterministic order. Sort by (project, status, name, version) for a stable report
        // (and stable `--json`); the status rank puts ready-to-adopt updates last.
        self.items.sort_by(|a, b| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.status.sort_rank().cmp(&b.status.sort_rank()))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.current.cmp(&b.current))
        });
        let summary = summarize(&self.items);
        OutdatedOutcome {
            summary,
            items: self.items,
            warnings: self.warnings,
            errors: self.errors,
            exit: Exit::Ok,
        }
    }

    async fn run_project(&mut self, pctx: &'a super::ProjectCtx) {
        let Some(read) = self.ws.read_project_ctx(pctx, self.opts) else {
            return;
        };

        if !self.refresh_lock(pctx, &read.project_label).await {
            return;
        }

        self.opts.progress.say(&format!(
            "Resolving {} dependencies ({})…",
            read.project_label, pctx.tool
        ));
        let mut deps = match self
            .ws
            .dependencies_in_scope(read.adapter, pctx, self.scope, self.opts)
            .await
        {
            Ok(deps) => deps,
            Err(error) => {
                tracing::warn!(
                    project = read.project_label,
                    tool = pctx.tool.as_str(),
                    error = %error,
                    "could not enumerate dependencies"
                );
                self.errors.push(diag_from_error(
                    &error,
                    pctx.tool,
                    &read.project_label,
                    None,
                ));
                return;
            }
        };
        // Build-backend requirements (`[build-system].requires`, e.g. hatchling) the lockfile never
        // records: merge them in so `outdated` surfaces a build-backend update the same way Dependabot
        // does. They are direct, so they belong in both the direct and the transitive view. A failure
        // to read them is a non-fatal warning — the resolved deps still report.
        let mut manifest_only = HashSet::new();
        match self
            .ws
            .manifest_constraints_in_scope(read.adapter, pctx, self.opts)
            .await
        {
            Ok(constraints) => {
                manifest_only.extend(constraints.iter().map(|dep| dep.package.clone()));
                deps.extend(constraints);
            }
            Err(error) => {
                tracing::warn!(
                    project = read.project_label,
                    tool = pctx.tool.as_str(),
                    error = %error,
                    "could not read build-system requirements"
                );
                self.warnings.push(diag_from_error(
                    &error,
                    pctx.tool,
                    &read.project_label,
                    None,
                ));
            }
        }
        let fetched = self
            .fetch_releases(read.adapter, pctx, &read.project_label, deps, &read.fetch)
            .await;

        let (mut project_items, verification_candidates) =
            self.classify_fetched(pctx, &read.project_label, &read.resolve, fetched);

        // Reconcile the per-package "adoptable" verdicts with what the whole-graph upgrade resolve would
        // actually land. A matured candidate the resolve cannot place (a conflicting requirement wins) is
        // re-classified `blocked`, so `outdated` predicts `upgrade` exactly instead of promising an
        // upgrade that would silently be held. Runs only when there is something to verify.
        self.verify_blocked(
            pctx,
            &read.project_label,
            &mut project_items,
            verification_candidates,
            manifest_only,
        )
        .await;
        self.items.extend(project_items);
    }

    fn classify_fetched(
        &mut self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        rctx: &cooldown_core::ResolveContext<'_>,
        fetched: Vec<(Dependency, cooldown_core::Result<Vec<Release>>)>,
    ) -> (Vec<OutdatedItem>, Vec<VerificationCandidate>) {
        let mut items = Vec::new();
        let mut candidates = Vec::new();
        for (dep, result) in fetched {
            match result {
                Ok(releases) => {
                    if is_yanked_locked(&releases, &dep) {
                        self.warnings
                            .push(yanked_warning(pctx, project_label, &dep));
                    }
                    let item = self.outdated_item(pctx, project_label, &dep, &releases, rctx);
                    if item.status == OutdatedStatus::Adoptable
                        && let Some(change) = adoptable_change(&dep, &releases, &item)
                    {
                        candidates.push(VerificationCandidate {
                            item_index: items.len(),
                            change,
                        });
                    }
                    items.push(item);
                }
                Err(error) => {
                    let diag =
                        diag_from_error(&error, pctx.tool, project_label, Some(&dep.package.name));
                    items.push(error_item(pctx, project_label, &dep, diag));
                }
            }
        }
        (items, candidates)
    }

    /// Re-classify any `adoptable` item the whole-graph upgrade resolve would not actually land as
    /// `blocked`, carrying the matured target it cannot reach and the blocker holding it out.
    ///
    /// Runs the upgrade executor's complete policy trial over a temporary project copy: native
    /// resolve, post-apply graph gate, transitive reconciliation, and residual candidate isolation.
    /// When that trial completes, its skipped set is exactly what `upgrade` would hold, while the
    /// real project is never read for mutation or written. An incomplete trial fails open and leaves
    /// the per-package verdicts unchanged. Skips entirely when no candidate is adoptable or the tool
    /// has no mutator.
    async fn verify_blocked(
        &mut self,
        pctx: &'a super::ProjectCtx,
        project_label: &str,
        items: &mut [OutdatedItem],
        candidates: Vec<VerificationCandidate>,
        manifest_only: HashSet<PackageId>,
    ) {
        if candidates.is_empty() {
            return;
        }
        if self.opts.offline {
            tracing::debug!(
                project = project_label,
                tool = pctx.tool.as_str(),
                "skipping upgrade-resolve verification in offline mode"
            );
            return;
        }
        if self.ws.mutator(pctx.tool).is_none() {
            return;
        }

        self.opts.progress.say(&format!(
            "Verifying {} adoptable update(s) in {} against the upgrade policy…",
            candidates.len(),
            project_label,
        ));
        let changes = candidates
            .iter()
            .map(|candidate| candidate.change.clone())
            .collect();
        let preview = self
            .ws
            .preview_project_upgrade(pctx, self.opts, changes, manifest_only)
            .await;
        let mut verification_errors = preview.errors;
        verification_errors.extend(preview.items.iter().filter_map(|item| item.error.clone()));
        self.warnings.extend(preview.warnings);
        if !verification_errors.is_empty() {
            // An incomplete probe cannot turn an adoptable candidate into a false `blocked` row.
            tracing::warn!(
                project = project_label,
                tool = pctx.tool.as_str(),
                errors = verification_errors.len(),
                "could not verify adoptable updates against the complete upgrade policy"
            );
            self.warnings.extend(verification_errors);
            return;
        }
        let held = held_from_preview(preview.items);
        apply_held(items, &candidates, &held);
    }

    async fn refresh_lock(&mut self, pctx: &'a super::ProjectCtx, project_label: &str) -> bool {
        match self
            .ws
            .refresh_project_lock(pctx, self.opts, project_label)
            .await
        {
            Ok(Some(report)) => self.handle_lock_report(report, pctx, project_label),
            Ok(None) => true,
            Err(error) => {
                self.errors.push(
                    diag_from_error(&error, pctx.tool, project_label, None)
                        .with_path(pctx.project.manifest.as_str()),
                );
                false
            }
        }
    }

    fn handle_lock_report(
        &mut self,
        report: LockVerifyReport,
        pctx: &'a super::ProjectCtx,
        project_label: &str,
    ) -> bool {
        let outcome = lock_report_outcome(
            report,
            pctx.tool,
            project_label,
            &pctx.project.manifest,
            self.opts.allow_stale_lock,
        );
        match outcome.action {
            LockReportAction::Continue => {
                if let Some(diagnostic) = outcome.diagnostic {
                    self.warnings.push(diagnostic);
                }
                true
            }
            LockReportAction::Skip => {
                if let Some(diagnostic) = outcome.diagnostic {
                    self.errors.push(diagnostic);
                }
                false
            }
        }
    }

    /// Fetch release metadata for every in-scope dependency of one project, bounded by the
    /// registry fan-out. Each release fetch is independent, so a single slow or failing dependency
    /// never blocks the others. The surrounding `info!`/per-dependency `debug!`/`trace!` spans make
    /// a stalled network call visible under `--log-level debug`.
    async fn fetch_releases(
        &self,
        adapter: &dyn cooldown_core::ToolRead,
        pctx: &super::ProjectCtx,
        project_label: &str,
        deps: Vec<Dependency>,
        fctx: &cooldown_core::FetchContext<'_>,
    ) -> Vec<(Dependency, cooldown_core::Result<Vec<Release>>)> {
        tracing::info!(
            project = project_label,
            tool = pctx.tool.as_str(),
            deps = deps.len(),
            fanout = self.opts.fanout(),
            "fetching release metadata"
        );
        self.opts.progress.say(&format!(
            "Fetching release metadata for {} dependencies…",
            deps.len()
        ));
        let started = std::time::Instant::now();
        let fetched = self
            .ws
            .fetch_candidate_releases(
                adapter,
                deps,
                fctx,
                self.opts.candidate_scope(),
                self.opts.fanout(),
            )
            .await;
        for (dep, result) in &fetched {
            match result {
                Ok(releases) => tracing::trace!(
                    package = dep.package.name.as_str(),
                    releases = releases.len(),
                    "fetched releases"
                ),
                Err(error) => tracing::debug!(
                    package = dep.package.name.as_str(),
                    error = %error,
                    "release fetch failed"
                ),
            }
        }
        tracing::info!(
            project = project_label,
            tool = pctx.tool.as_str(),
            fetched = fetched.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "fetched release metadata"
        );
        fetched
    }

    /// Build the report item for a dependency whose releases were fetched successfully.
    fn outdated_item(
        &self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        dep: &Dependency,
        releases: &[Release],
        rctx: &cooldown_core::ResolveContext<'_>,
    ) -> OutdatedItem {
        let verdict = evaluate(dep, releases, &pctx.policy.layers, rctx, self.ws.now());

        // The candidate whose cooldown the report displays: the newest by default, or the soonest to
        // mature under `--countdown soonest`. Both the `window` and `candidate_age_days` below read
        // from this one candidate so the cooldown cell stays internally consistent (`age/window`).
        let shown = verdict.cooldown_candidate(self.opts.cooldown_horizon, self.ws.now());

        // Prefer the shown candidate's window; fall back to a current-pin resolution when there is
        // no candidate at all (up to date / commit pin).
        let window = if let Some(candidate) = shown {
            render_window(&candidate.window, self.ws.now())
        } else {
            let q = ResolveQuery {
                tool: pctx.tool,
                package: &dep.package.name,
                registry: dep.package.registry.as_deref(),
                project: &pctx.rel_path,
                kind: ResolveKind::CurrentPin,
            };
            render_window(
                &resolve(&pctx.policy.layers, &q, self.ws.now()).window,
                self.ws.now(),
            )
        };

        // The age of the shown candidate — the version whose `window` is shown above — so the
        // report can read its cooldown position as `age/window` (e.g. `13d/14d`, almost adoptable).
        let candidate_age_days = shown
            .and_then(|candidate| candidate.published_at)
            .map(|published| age_days(published, self.ws.now()));

        // Under `--countdown soonest` the cooldown can count down to a version no other column names
        // (an intermediate that matures before the newest candidate). Label that version so the cell
        // is unambiguous; in the default `latest` view the shown candidate *is* the newest one, so
        // there is nothing to add. Compare against the newest candidate (not `verdict.latest`, which
        // also counts an unclassifiable newest release that never became a candidate) so the default
        // view stays byte-identical: under `Latest`, `shown` is exactly `candidates.last()`.
        let newest = verdict
            .candidates
            .last()
            .map(|candidate| &candidate.version);
        let cooldown_version = shown
            .map(|candidate| &candidate.version)
            .filter(|&version| Some(version) != newest)
            .map(ToString::to_string);

        let latest = verdict.latest.as_ref().map(|lv| {
            let published = releases
                .iter()
                .find(|r| &r.version == lv)
                .and_then(|r| r.published_at);
            LatestInfo {
                version: lv.to_string(),
                published_at: published.map(|p| p.to_string()),
                age_days: published.map(|p| age_days(p, self.ws.now())),
            }
        });

        OutdatedItem {
            name: dep.package.name.clone(),
            tool: pctx.tool.as_str().to_string(),
            project: project_label.to_string(),
            registry: dep.package.registry.clone(),
            direct: dep.direct,
            current: dep.current.to_string(),
            members: dep.members.clone(),
            window,
            candidate_age_days,
            cooldown_version,
            status: verdict.status.into(),
            adoptable_target: verdict.adoptable_target.map(|v| v.to_string()),
            blocked_by: None,
            latest,
            error: None,
        }
    }
}

/// Whether the dependency's currently-locked version is marked yanked among `releases`.
fn is_yanked_locked(releases: &[Release], dep: &Dependency) -> bool {
    releases
        .iter()
        .any(|r| r.version == dep.current && r.yanked)
}

fn yanked_warning(pctx: &super::ProjectCtx, project_label: &str, dep: &Dependency) -> Diagnostic {
    Diagnostic::new(
        cooldown_core::DiagnosticKind::Yanked,
        "locked version is yanked",
    )
    .with_tool(pctx.tool.as_str())
    .with_project(project_label)
    .with_package(&dep.package.name)
    .with_version(dep.current.as_str())
}

fn error_item(
    pctx: &super::ProjectCtx,
    project_label: &str,
    dep: &Dependency,
    diag: Diagnostic,
) -> OutdatedItem {
    OutdatedItem {
        name: dep.package.name.clone(),
        tool: pctx.tool.as_str().to_string(),
        project: project_label.to_string(),
        registry: dep.package.registry.clone(),
        direct: dep.direct,
        current: dep.current.to_string(),
        members: dep.members.clone(),
        window: Window {
            min_age_days: 0.0,
            source: "n/a".into(),
            clamped_by: None,
        },
        candidate_age_days: None,
        cooldown_version: None,
        status: OutdatedStatus::Error,
        adoptable_target: None,
        blocked_by: None,
        latest: None,
        error: Some(diag),
    }
}

/// An `adoptable` item paired (by position in the project's item list) with the forward [`Change`]
/// the policy preview verifies for it.
struct VerificationCandidate {
    item_index: usize,
    change: Change,
}

/// The forward [`Change`] an `adoptable` item would take — its matured target. `None` when the item
/// has no target or the target equals the current pin (nothing to verify). The kind is informational
/// for verification (only landed-or-held matters), so the neutral [`UpdateKind::Minor`] is used.
fn adoptable_change(dep: &Dependency, releases: &[Release], item: &OutdatedItem) -> Option<Change> {
    let target = item.adoptable_target.as_ref()?;
    if *target == item.current {
        return None;
    }
    let target = Version::new(target.clone());
    Some(Change {
        package: super::upgrade::target_package_for(releases, dep, &target),
        from: Version::new(item.current.clone()),
        to: target,
        kind: UpdateKind::Minor,
        downgrade: false,
        direct: item.direct,
        members: item.members.clone(),
    })
}

/// The change identity of a preview report row — [`change_target_key`] over the row's fields, so
/// preview outcomes and verification candidates key identically.
fn held_key_for_upgrade(item: &UpgradeItem) -> ChangeTargetKey {
    change_target_key_parts(
        &item.name,
        item.registry.as_deref(),
        &item.to,
        item.direct,
        &item.members,
    )
}

/// The preview's held set: every skipped row keyed by change identity, mapped to the named
/// blocker (`None` when the row blames itself — the generic "resolver rejected" form).
fn held_from_preview(preview: Vec<UpgradeItem>) -> HashMap<ChangeTargetKey, Option<String>> {
    let mut held = HashMap::new();
    for item in preview {
        let key = held_key_for_upgrade(&item);
        let Some(skipped) = item.skipped else {
            continue;
        };
        let blocker = skipped.offending.filter(|offender| *offender != item.name);
        held.insert(key, blocker);
    }
    held
}

/// Re-classify every `adoptable` item the upgrade resolve could not land (`held`) as `blocked`,
/// carrying the blocker the resolve named. An item the resolve landed (absent from `held`) keeps its
/// `adoptable` verdict, so `outdated`'s blocked set is exactly `upgrade`'s held set.
fn apply_held(
    items: &mut [OutdatedItem],
    candidates: &[VerificationCandidate],
    held: &HashMap<ChangeTargetKey, Option<String>>,
) {
    for candidate in candidates {
        if let Some(blocker) = held.get(&change_target_key(&candidate.change))
            && let Some(item) = items.get_mut(candidate.item_index)
            && item.status == OutdatedStatus::Adoptable
        {
            item.status = OutdatedStatus::Blocked;
            item.blocked_by = blocker.clone();
        }
    }
}

fn summarize(items: &[OutdatedItem]) -> OutdatedSummary {
    let mut s = OutdatedSummary {
        total: items.len(),
        adoptable: 0,
        blocked: 0,
        in_cooldown: 0,
        up_to_date: 0,
        exempt: 0,
        held: 0,
        unknown_age: 0,
        errors: 0,
    };
    for it in items {
        match it.status {
            OutdatedStatus::Adoptable => s.adoptable += 1,
            OutdatedStatus::Blocked => s.blocked += 1,
            OutdatedStatus::InCooldown | OutdatedStatus::CurrentInCooldown => {
                s.in_cooldown += 1;
            }
            OutdatedStatus::UpToDate => s.up_to_date += 1,
            OutdatedStatus::Exempt => s.exempt += 1,
            OutdatedStatus::Held => s.held += 1,
            OutdatedStatus::UnknownAge => s.unknown_age += 1,
            OutdatedStatus::Error => s.errors += 1,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{
        VerificationCandidate, adoptable_change, apply_held, change_target_key, held_from_preview,
    };
    use crate::app::{OutdatedItem, OutdatedStatus, SkippedInfo, UpgradeItem, Window};
    use cooldown_core::{
        Dependency, MajorKey, MemberRef, PackageId, Release, ReleaseOrder, ReleaseQuality,
        SkipReason, ToolId, UpdateKind, Version,
    };

    const UV: ToolId = ToolId("uv");
    const GO: ToolId = ToolId("go");

    fn adoptable(name: &str, current: &str, target: &str) -> OutdatedItem {
        OutdatedItem {
            name: name.to_string(),
            tool: "uv".to_string(),
            project: ".".to_string(),
            registry: Some("https://pypi.org/simple".to_string()),
            direct: true,
            current: current.to_string(),
            members: Vec::new(),
            window: Window {
                min_age_days: 14.0,
                source: "default".into(),
                clamped_by: None,
            },
            candidate_age_days: Some(40.0),
            cooldown_version: None,
            status: OutdatedStatus::Adoptable,
            adoptable_target: Some(target.to_string()),
            blocked_by: None,
            latest: None,
            error: None,
        }
    }

    fn skipped(name: &str, current: &str, target: &str, blocker: &str) -> UpgradeItem {
        UpgradeItem {
            name: name.to_string(),
            tool: "uv".to_string(),
            project: ".".to_string(),
            direct: true,
            downgrade: false,
            members: Vec::new(),
            registry: Some("https://pypi.org/simple".to_string()),
            from: current.to_string(),
            to: target.to_string(),
            kind: UpdateKind::Minor,
            applied: false,
            skipped: Some(SkippedInfo {
                reason: SkipReason::ResolverConflict,
                message: format!("conflicts with {blocker}"),
                offending: Some(blocker.to_string()),
            }),
            error: None,
        }
    }

    fn dependency(item: &OutdatedItem) -> Dependency {
        Dependency {
            package: PackageId::new(UV, item.name.clone(), item.registry.clone()),
            current: Version::new(item.current.clone()),
            current_quality: ReleaseQuality::Stable,
            direct: item.direct,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            members: item.members.clone(),
            pinned: false,
        }
    }

    fn candidate(item_index: usize, item: &OutdatedItem) -> VerificationCandidate {
        VerificationCandidate {
            item_index,
            change: adoptable_change(&dependency(item), &[], item).expect("a forward change"),
        }
    }

    fn release(version: &str, order: u8, major: &str) -> Release {
        Release {
            version: Version::new(version),
            order: ReleaseOrder(vec![order]),
            major: MajorKey(major.to_string()),
            kind_from_current: Some(UpdateKind::Major),
            published_at: None,
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
    }

    #[test]
    fn adoptable_change_skips_a_noop_target() {
        // A target equal to the current pin is not a move, so it is never verified.
        let item = adoptable("typer", "0.26.7", "0.26.7");
        assert!(adoptable_change(&dependency(&item), &[], &item).is_none());

        let item = adoptable("typer", "0.25.1", "0.26.7");
        let change = adoptable_change(&dependency(&item), &[], &item).expect("a forward change");
        assert_eq!(change.from, Version::new("0.25.1"));
        assert_eq!(change.to, Version::new("0.26.7"));
        assert!(!change.downgrade);
    }

    #[test]
    fn adoptable_change_uses_the_target_go_path_major() {
        let item = adoptable("example.com/foo", "v1.9.0", "v2.0.0");
        let mut dep = dependency(&item);
        dep.package = PackageId::new(GO, "example.com/foo", None);
        let releases = [release("v1.9.0", 1, ""), release("v2.0.0", 2, "/v2")];

        let change = adoptable_change(&dep, &releases, &item).expect("a cross-major change");

        assert_eq!(change.package.name, "example.com/foo/v2");
    }

    #[test]
    fn conflicting_candidate_is_reclassified_blocked_with_its_blocker() {
        let mut items = vec![
            adoptable("typer", "0.25.1", "0.26.7"),
            adoptable("requests", "2.34.1", "2.34.2"),
        ];
        let candidates = vec![candidate(0, &items[0]), candidate(1, &items[1])];
        let held = held_from_preview(vec![skipped(
            "typer",
            "0.25.1",
            "0.26.7",
            "huggingface-hub",
        )]);

        assert_eq!(
            held.get(&change_target_key(&candidates[0].change)),
            Some(&Some("huggingface-hub".to_string()))
        );
        assert!(!held.contains_key(&change_target_key(&candidates[1].change)));

        apply_held(&mut items, &candidates, &held);
        let typer = items.iter().find(|i| i.name == "typer").expect("typer");
        assert_eq!(typer.status, OutdatedStatus::Blocked);
        assert_eq!(typer.adoptable_target.as_deref(), Some("0.26.7"));
        assert_eq!(typer.blocked_by.as_deref(), Some("huggingface-hub"));

        let requests = items
            .iter()
            .find(|i| i.name == "requests")
            .expect("requests");
        assert_eq!(requests.status, OutdatedStatus::Adoptable);
        assert_eq!(requests.blocked_by, None);
    }

    #[test]
    fn held_candidate_does_not_block_same_package_at_a_different_current_version() {
        let mut items = vec![
            adoptable("serde", "0.9.0", "0.9.15"),
            adoptable("serde", "1.0.188", "1.0.190"),
        ];
        let candidates = vec![candidate(0, &items[0]), candidate(1, &items[1])];
        let held = held_from_preview(vec![skipped(
            "serde",
            "0.9.0",
            "0.9.15",
            "legacy-framework",
        )]);

        assert!(held.contains_key(&change_target_key(&candidates[0].change)));
        assert!(!held.contains_key(&change_target_key(&candidates[1].change)));

        apply_held(&mut items, &candidates, &held);

        assert_eq!(items[0].status, OutdatedStatus::Blocked);
        assert_eq!(items[0].blocked_by.as_deref(), Some("legacy-framework"));
        assert_eq!(items[1].status, OutdatedStatus::Adoptable);
        assert_eq!(items[1].blocked_by, None);
    }

    #[test]
    fn held_candidate_does_not_block_a_different_target() {
        let mut items = vec![adoptable("serde", "1.0.188", "1.0.190")];
        let candidates = vec![candidate(0, &items[0])];
        let held = held_from_preview(vec![skipped(
            "serde",
            "1.0.188",
            "1.0.189",
            "legacy-framework",
        )]);

        apply_held(&mut items, &candidates, &held);

        assert_eq!(items[0].status, OutdatedStatus::Adoptable);
        assert_eq!(items[0].blocked_by, None);
    }

    #[test]
    fn held_candidate_does_not_block_a_different_workspace_member() {
        let mut left = adoptable("serde", "1.0.188", "1.0.190");
        left.members = vec![MemberRef {
            name: "left".to_string(),
            path: "packages/left".to_string(),
        }];
        let mut right = left.clone();
        right.members = vec![MemberRef {
            name: "right".to_string(),
            path: "packages/right".to_string(),
        }];
        let mut items = vec![left, right];
        let candidates = vec![candidate(0, &items[0]), candidate(1, &items[1])];
        let mut skipped = skipped("serde", "1.0.188", "1.0.190", "legacy-framework");
        skipped.members = items[0].members.clone();
        let held = held_from_preview(vec![skipped]);

        apply_held(&mut items, &candidates, &held);

        assert_eq!(items[0].status, OutdatedStatus::Blocked);
        assert_eq!(items[1].status, OutdatedStatus::Adoptable);
    }
}
