//! `outdated` — what could update, split into "adoptable now" vs "in cooldown". Reasons over the
//! candidate set; informational, so per-dep failures never change the exit code.

use super::{
    Exit, LockReportAction, RunOpts, Workspace, age_days, diag_from_error, lock_report_outcome,
    render_window,
};
use super::{LatestInfo, OutdatedItem, OutdatedStatus, OutdatedSummary, Window};
use cooldown_core::{
    Change, DepScope, Dependency, Diagnostic, LockVerifyReport, PackageId, Plan, Project, Release,
    ResolveKind, ResolveQuery, UpdateKind, Version, evaluate, resolve,
};
use std::collections::HashMap;

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
        match self
            .ws
            .manifest_constraints_in_scope(read.adapter, pctx, self.opts)
            .await
        {
            Ok(constraints) => deps.extend(constraints),
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

        let mut project_items = Vec::new();
        for (dep, result) in fetched {
            match result {
                Ok(releases) => {
                    if is_yanked_locked(&releases, &dep) {
                        self.warnings
                            .push(yanked_warning(pctx, &read.project_label, &dep));
                    }
                    project_items.push(self.outdated_item(
                        pctx,
                        &read.project_label,
                        &dep,
                        &releases,
                        &read.resolve,
                    ));
                }
                Err(error) => {
                    let diag = diag_from_error(
                        &error,
                        pctx.tool,
                        &read.project_label,
                        Some(&dep.package.name),
                    );
                    project_items.push(error_item(pctx, &read.project_label, &dep, diag));
                }
            }
        }

        // Reconcile the per-package "adoptable" verdicts with what the whole-graph upgrade resolve would
        // actually land. A matured candidate the resolve cannot place (a conflicting requirement wins) is
        // re-classified `blocked`, so `outdated` predicts `upgrade` exactly instead of promising an
        // upgrade that would silently be held. Runs only when there is something to verify.
        self.verify_blocked(pctx, &read.project_label, &mut project_items)
            .await;
        self.items.extend(project_items);
    }

    /// Re-classify any `adoptable` item the whole-graph upgrade resolve would not actually land as
    /// `blocked`, carrying the matured target it cannot reach and the blocker holding it out.
    ///
    /// Reuses the upgrade's own resolve ([`ToolWrite::apply`], which drives the whole-graph
    /// `--upgrade` re-lock) over a temporary copy of the project so the real `uv.lock`/`pyproject.toml`
    /// are never touched. The resolve's `skipped` set is exactly `upgrade`'s held set, so the two
    /// commands agree by construction. Skips entirely when no candidate is adoptable (no resolve cost)
    /// or the tool has no mutator (nothing to verify against).
    async fn verify_blocked(
        &mut self,
        pctx: &'a super::ProjectCtx,
        project_label: &str,
        items: &mut [OutdatedItem],
    ) {
        let changes: Vec<Change> = items
            .iter()
            .filter(|item| item.status == OutdatedStatus::Adoptable)
            .filter_map(|item| adoptable_change(pctx.tool, item))
            .collect();
        if changes.is_empty() {
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
        let Some(writer) = self.ws.mutator(pctx.tool) else {
            return;
        };

        self.opts.progress.say(&format!(
            "Verifying {} adoptable update(s) in {} against the upgrade resolve…",
            changes.len(),
            project_label,
        ));
        let held = match resolve_held(writer, &pctx.project, changes, self.opts.rewrite).await {
            Ok(held) => held,
            Err(error) => {
                // A failed verification probe must not turn an adoptable into a false `blocked`: leave
                // the per-package verdicts intact and surface the failure as a warning.
                tracing::warn!(
                    project = project_label,
                    tool = pctx.tool.as_str(),
                    error = %error,
                    "could not verify adoptable updates against the upgrade resolve"
                );
                self.warnings
                    .push(diag_from_error(&error, pctx.tool, project_label, None));
                return;
            }
        };
        apply_held(items, &held);
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

/// The forward [`Change`] an `adoptable` item would take — its matured target. `None` when the item
/// has no target or the target equals the current pin (nothing to verify). The kind is informational
/// for verification (only landed-or-held matters), so the neutral [`UpdateKind::Minor`] is used.
fn adoptable_change(tool: cooldown_core::ToolId, item: &OutdatedItem) -> Option<Change> {
    let target = item.adoptable_target.as_ref()?;
    if *target == item.current {
        return None;
    }
    Some(Change {
        package: PackageId::new(tool, item.name.clone(), item.registry.clone()),
        from: Version::new(item.current.clone()),
        to: Version::new(target.clone()),
        kind: UpdateKind::Minor,
        downgrade: false,
        direct: item.direct,
        members: item.members.clone(),
    })
}

type HeldKey = (String, Option<String>, String);

fn held_key(name: String, registry: Option<String>, current: String) -> HeldKey {
    (name, registry, current)
}

fn held_key_for_change(change: &Change) -> HeldKey {
    held_key(
        change.package.name.clone(),
        change.package.registry.clone(),
        change.from.to_string(),
    )
}

fn held_key_for_item(item: &OutdatedItem) -> HeldKey {
    held_key(
        item.name.clone(),
        item.registry.clone(),
        item.current.clone(),
    )
}

/// Run the whole-graph upgrade resolve for `changes` against a throwaway copy of the project, and
/// return the candidates the resolve could **not** land — `upgrade`'s held set — keyed by package
/// name, registry, and current version, each mapped to the blocker the resolve named (when distinct
/// from the candidate itself).
///
/// The resolve reuses [`ToolWrite::apply`] (the same path `upgrade` commits), so the held set matches
/// `upgrade` by construction. It runs entirely inside a temporary directory copied from the project,
/// so the real `uv.lock`/`pyproject.toml` are never read for mutation or written.
async fn resolve_held(
    writer: &dyn cooldown_core::ToolWrite,
    project: &Project,
    changes: Vec<Change>,
    rewrite: cooldown_core::RewriteMode,
) -> cooldown_core::Result<HashMap<HeldKey, Option<String>>> {
    let copy = super::project_copy::ProjectCopy::create(project, &writer.resolve_inputs())?;
    let temp_project = &copy.project;

    let plan = Plan { changes, rewrite };
    let journal = writer.mutation_journal(temp_project, &plan).await?;
    // Resilient apply: one unfetchable/conflicting candidate must not poison the whole verify resolve
    // and make every adoptable candidate look blocked — it is isolated and the rest still resolve.
    let report =
        super::resilient_apply::apply_resilient(writer, temp_project, &plan, &journal).await?;

    let mut held = HashMap::new();
    for skipped in report.skipped {
        let candidate = skipped.change.package.name.clone();
        // Mirror the held-message policy: a blocker distinct from the candidate is named; the
        // candidate blaming itself is the generic "resolver rejected" form, so no blocker is surfaced.
        let blocker = skipped
            .offending
            .map(|package| package.name)
            .filter(|offender| *offender != candidate);
        held.insert(held_key_for_change(&skipped.change), blocker);
    }
    Ok(held)
}

/// Re-classify every `adoptable` item the upgrade resolve could not land (`held`) as `blocked`,
/// carrying the blocker the resolve named. An item the resolve landed (absent from `held`) keeps its
/// `adoptable` verdict, so `outdated`'s blocked set is exactly `upgrade`'s held set.
fn apply_held(items: &mut [OutdatedItem], held: &HashMap<HeldKey, Option<String>>) {
    for item in items.iter_mut() {
        if item.status != OutdatedStatus::Adoptable {
            continue;
        }
        if let Some(blocker) = held.get(&held_key_for_item(item)) {
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
    use super::{adoptable_change, apply_held, held_key_for_item, resolve_held};
    use crate::app::{OutdatedItem, OutdatedStatus, Window};
    use async_trait::async_trait;
    use cooldown_core::{
        ApplyReport, PackageId, Plan, Project, ProjectMutationJournal, Result, RewriteMode,
        SkipReason, Skipped, ToolId, ToolWrite, VerifyReport, Version,
    };

    const UV: ToolId = ToolId("uv");

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

    /// A `ToolWrite` whose `apply` reports a fixed held set, so the verification path can be exercised
    /// without spawning a real resolver. It holds `held` (offending `holder`) and lands everything else.
    struct FakeWriter {
        held: &'static str,
        held_current: &'static str,
        holder: &'static str,
    }

    #[async_trait]
    impl ToolWrite for FakeWriter {
        async fn mutation_journal(
            &self,
            _project: &Project,
            _plan: &Plan,
        ) -> Result<ProjectMutationJournal> {
            Ok(ProjectMutationJournal::default())
        }

        async fn apply(
            &self,
            _project: &Project,
            plan: &Plan,
            _journal: &ProjectMutationJournal,
        ) -> Result<ApplyReport> {
            let mut report = ApplyReport::default();
            for change in &plan.changes {
                if change.package.name == self.held && change.from.as_str() == self.held_current {
                    report.skipped.push(Skipped {
                        change: change.clone(),
                        reason: SkipReason::ResolverConflict,
                        offending: Some(PackageId::new(
                            UV,
                            self.holder.to_string(),
                            Some("https://pypi.org/simple".to_string()),
                        )),
                    });
                } else {
                    report.applied.push(change.clone());
                }
            }
            Ok(report)
        }

        async fn build(&self, _project: &Project) -> Result<VerifyReport> {
            Ok(VerifyReport {
                ok: true,
                detail: String::new(),
            })
        }
    }

    fn temp_project() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let manifest = root.join("pyproject.toml");
        std::fs::write(
            &manifest,
            "[project]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("write manifest");
        std::fs::write(root.join("uv.lock"), "version = 1\nrevision = 3\n").expect("write lock");
        let project = Project {
            root,
            kind: UV,
            manifest,
            exclude_newer: None,
        };
        (dir, project)
    }

    #[test]
    fn adoptable_change_skips_a_noop_target() {
        // A target equal to the current pin is not a move, so it is never verified.
        let item = adoptable("typer", "0.26.7", "0.26.7");
        assert!(adoptable_change(UV, &item).is_none());

        let item = adoptable("typer", "0.25.1", "0.26.7");
        let change = adoptable_change(UV, &item).expect("a forward change");
        assert_eq!(change.from, Version::new("0.25.1"));
        assert_eq!(change.to, Version::new("0.26.7"));
        assert!(!change.downgrade);
    }

    #[tokio::test]
    async fn conflicting_candidate_is_reclassified_blocked_with_its_blocker() {
        // typer's matured 0.26.7 is adoptable in isolation, but the whole-graph resolve holds it
        // because huggingface-hub requires typer<0.26.0. `outdated` must report it `blocked` (named),
        // matching what `upgrade` reports `held` — not a phantom `adoptable`. `requests` lands, so it
        // stays adoptable: only the candidate the resolve could not place is re-classified.
        let writer = FakeWriter {
            held: "typer",
            held_current: "0.25.1",
            holder: "huggingface-hub",
        };
        let (_dir, project) = temp_project();
        let mut items = vec![
            adoptable("typer", "0.25.1", "0.26.7"),
            adoptable("requests", "2.34.1", "2.34.2"),
        ];
        let changes: Vec<_> = items
            .iter()
            .filter_map(|item| adoptable_change(UV, item))
            .collect();
        let held = resolve_held(&writer, &project, changes, RewriteMode::Auto)
            .await
            .expect("resolve");

        // The held set is exactly the candidate the resolve could not land, naming the blocker.
        assert_eq!(
            held.get(&held_key_for_item(&items[0])),
            Some(&Some("huggingface-hub".to_string()))
        );
        assert!(!held.contains_key(&held_key_for_item(&items[1])));

        apply_held(&mut items, &held);
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

    #[tokio::test]
    async fn held_candidate_does_not_block_same_package_at_a_different_current_version() {
        let writer = FakeWriter {
            held: "serde",
            held_current: "0.9.0",
            holder: "legacy-framework",
        };
        let (_dir, project) = temp_project();
        let mut items = vec![
            adoptable("serde", "0.9.0", "0.9.15"),
            adoptable("serde", "1.0.188", "1.0.190"),
        ];
        let changes: Vec<_> = items
            .iter()
            .filter_map(|item| adoptable_change(UV, item))
            .collect();
        let held = resolve_held(&writer, &project, changes, RewriteMode::Auto)
            .await
            .expect("resolve");

        assert!(held.contains_key(&held_key_for_item(&items[0])));
        assert!(!held.contains_key(&held_key_for_item(&items[1])));

        apply_held(&mut items, &held);

        assert_eq!(items[0].status, OutdatedStatus::Blocked);
        assert_eq!(items[0].blocked_by.as_deref(), Some("legacy-framework"));
        assert_eq!(items[1].status, OutdatedStatus::Adoptable);
        assert_eq!(items[1].blocked_by, None);
    }
}
