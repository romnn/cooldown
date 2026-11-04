//! `sync` — write the resolved cooldown policy into each project's native config (uv
//! `exclude-newer`, …), so `cooldown.toml` is the single source of truth and even a bare `uv sync`
//! someone runs by hand still respects the window.
//! Tools without a native cooldown concept (Go,
//! Cargo) report `unsupported`; nothing is written for them.

use super::lock::{ProjectReadGuard, ProjectWriteGuard, RepoToolReadGuard, RepoToolWriteGuard};
use super::{Exit, RunOpts, Workspace, diag_from_error, recovery_diagnostics};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::fs::ManifestFamily;
use cooldown_core::{
    Diagnostic, NativeExemptions, PolicyContext, ResolveKind, ResolveQuery, ResolvedPolicy,
    SyncReport, SyncScope, ToolId, ToolWrite, WindowSpec, native_exemptions, resolve,
};

enum SyncAccessGuard {
    Read {
        #[expect(dead_code, reason = "the field keeps the project read lease alive")]
        guard: ProjectReadGuard,
    },
    Write {
        #[expect(dead_code, reason = "the field keeps the project write lease alive")]
        guard: ProjectWriteGuard,
    },
}

struct SyncAccess {
    guard: SyncAccessGuard,
    recovery: Vec<Diagnostic>,
}

enum RepoSyncResourceGuard {
    Read {
        #[expect(dead_code, reason = "the field keeps the repository read lease alive")]
        guard: RepoToolReadGuard,
    },
    Write {
        #[expect(dead_code, reason = "the field keeps the repository write lease alive")]
        guard: RepoToolWriteGuard,
    },
}

struct RepoSyncAccess {
    #[expect(
        dead_code,
        reason = "the field keeps the repository resource lease alive"
    )]
    resource: RepoSyncResourceGuard,
    #[expect(
        dead_code,
        reason = "the field keeps every consumer project lease alive"
    )]
    projects: Vec<SyncAccessGuard>,
}

/// What happened when syncing one project's native config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    /// The native config was rewritten to match the policy.
    Written,
    /// The native config already matched the policy; nothing was rewritten.
    Unchanged,
    /// The tool has no native cooldown config to write into.
    Unsupported,
    /// Syncing this project failed.
    Error,
}

impl SyncStatus {
    /// The lowercase token used in text and JSON output.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            SyncStatus::Written => "written",
            SyncStatus::Unchanged => "unchanged",
            SyncStatus::Unsupported => "unsupported",
            SyncStatus::Error => "error",
        }
    }
}

/// One project's sync result.
#[derive(Debug, Clone)]
pub struct SyncItem {
    /// The tool whose native config was synced.
    pub tool: String,
    /// The project, relative to the repo root.
    pub project: String,
    /// The outcome for this project.
    pub status: SyncStatus,
    /// The native config file written or checked, when applicable, relative to the directory
    /// `project` names and `/`-separated.
    pub path: Option<String>,
    /// The policy window synced (e.g. `14d`), for display.
    pub window: Option<String>,
    /// The diagnostic, when [`status`](SyncItem::status) is [`SyncStatus::Error`].
    pub error: Option<Diagnostic>,
}

/// Per-status counts across all synced projects.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncSummary {
    /// Projects whose native config was rewritten.
    pub written: usize,
    /// Projects already in sync.
    pub unchanged: usize,
    /// Projects whose tool has no native cooldown config.
    pub unsupported: usize,
    /// Projects that failed to sync.
    pub errors: usize,
}

impl SyncSummary {
    /// Tally one non-error outcome.
    /// An [`SyncStatus::Error`] is counted at the failing call site
    /// (which also carries the diagnostic), so it is a no-op here.
    fn record(&mut self, status: SyncStatus) {
        match status {
            SyncStatus::Written => self.written += 1,
            SyncStatus::Unchanged => self.unchanged += 1,
            SyncStatus::Unsupported => self.unsupported += 1,
            SyncStatus::Error => {}
        }
    }
}

/// The result of `sync`: the per-project findings, counts, and the process exit.
pub struct SyncOutcome {
    /// Per-status counts.
    pub summary: SyncSummary,
    /// The per-project results.
    pub items: Vec<SyncItem>,
    /// Recovery notices, admission evaluation failures, and other non-fatal diagnostics.
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors (none today; per-project failures live on their [`SyncItem`]).
    pub errors: Vec<Diagnostic>,
    /// The process exit: non-zero if any project failed to sync.
    pub exit: Exit,
}

fn scoped_tools(workspace: &Workspace, opts: &RunOpts) -> Vec<ToolId> {
    let mut tools = Vec::new();
    for project in workspace.scoped_projects(opts) {
        if !tools.contains(&project.tool) {
            tools.push(project.tool);
        }
    }
    tools
}

fn unsupported_item(tool: ToolId) -> SyncItem {
    SyncItem {
        tool: tool.as_str().to_string(),
        project: repo_relative_root(),
        status: SyncStatus::Unsupported,
        path: None,
        window: None,
        error: None,
    }
}

impl Workspace {
    /// Write the resolved cooldown policy down into native config, dispatching on each tool's
    /// [`SyncScope`].
    ///
    /// A [`SyncScope::Project`] tool is synced per in-scope project (its manifest's native field).
    /// A [`SyncScope::Repo`] tool's single repo-level file (uv's root `uv.toml`) is resolved against
    /// the repo-wide cascade and written **exactly once per tool**, no matter how many of its
    /// projects are in scope.
    /// Its write lease covers every detected project that consumes that shared file.
    /// A [`SyncScope::None`] tool reports a single `unsupported` item.
    ///
    /// Idempotent: a target already in sync is reported `unchanged` and not rewritten.
    /// Fail-soft: a
    /// write failure becomes one `error` item (and a non-zero exit) without aborting the rest.
    pub async fn sync(&self, opts: &RunOpts) -> SyncOutcome {
        let mut items = Vec::new();
        let mut summary = SyncSummary::default();
        let mut warnings = Vec::new();

        // The distinct in-scope tools, in first-seen order.
        // Each is handled once: a repo-scoped tool
        // is written exactly once (never per project), a project-scoped tool iterates its own
        // projects.
        // The final item list is sorted below, so this order is not load-bearing.
        let tools = scoped_tools(self, opts);

        for tool in tools {
            let Some(writer) = self.mutator(tool) else {
                self.note_projects(opts, tool, "checking native policy support");
                summary.unsupported += 1;
                items.push(unsupported_item(tool));
                continue;
            };

            match writer.sync_scope() {
                SyncScope::Project => {
                    for pctx in self.scoped_projects(opts).filter(|pctx| pctx.tool == tool) {
                        let project_progress = opts.progress.project(tool, pctx.rel_path.as_str());
                        project_progress.phase("syncing native policy");
                        items.push(
                            self.sync_project(
                                writer,
                                pctx,
                                tool,
                                opts,
                                &mut summary,
                                &mut warnings,
                            )
                            .await,
                        );
                    }
                }
                SyncScope::Repo => {
                    // One write serves every project, so one block stands for it on the
                    // display; the tool's other projects are counted complete afterwards.
                    let mut scoped_projects = self
                        .scoped_projects(opts)
                        .filter(|project| project.tool == tool);
                    let live = scoped_projects.next().map(|project| {
                        let project_progress =
                            opts.progress.project(tool, project.rel_path.as_str());
                        project_progress.phase("syncing repository-native policy");
                        project_progress
                    });
                    let protected_projects: Vec<_> = self
                        .projects()
                        .iter()
                        .filter(|project| project.tool == tool)
                        .collect();
                    items.push(
                        self.sync_repo(
                            writer,
                            tool,
                            opts,
                            &protected_projects,
                            &mut summary,
                            &mut warnings,
                        )
                        .await,
                    );
                    drop(live);
                    for project in scoped_projects {
                        opts.progress
                            .project(tool, project.rel_path.as_str())
                            .phase("synced repository-native policy");
                    }
                }
                SyncScope::None => {
                    self.note_projects(opts, tool, "checking native policy support");
                    summary.unsupported += 1;
                    items.push(unsupported_item(tool));
                }
            }
        }

        items.sort_by(|a, b| a.project.cmp(&b.project).then_with(|| a.tool.cmp(&b.tool)));
        let exit = if summary.errors > 0 {
            Exit::Environment
        } else {
            Exit::Ok
        };
        SyncOutcome {
            summary,
            items,
            warnings,
            errors: Vec::new(),
            exit,
        }
    }

    /// Opens each of `tool`'s in-scope projects in turn with `phase`, so the projects are
    /// counted complete one by one without stacking a block per project on the display for a
    /// message that is the same for all of them.
    fn note_projects(&self, opts: &RunOpts, tool: ToolId, phase: &str) {
        for project in self
            .scoped_projects(opts)
            .filter(|project| project.tool == tool)
        {
            opts.progress
                .project(tool, project.rel_path.as_str())
                .phase(phase);
        }
    }

    /// Sync one project's per-project native config ([`SyncScope::Project`]).
    async fn sync_project(
        &self,
        writer: &dyn cooldown_core::ToolWrite,
        pctx: &super::ProjectCtx,
        tool: ToolId,
        opts: &RunOpts,
        summary: &mut SyncSummary,
        warnings: &mut Vec<Diagnostic>,
    ) -> SyncItem {
        let project = pctx.rel_path.to_string();
        let window = native_default_window(&pctx.policy.layers, tool, &pctx.rel_path, self.now());
        // Bake the `latest`/`allow` package selectors that exempt every release into the native
        // per-package exemption list alongside the default window, so a cooldown-exempt package is
        // exempt natively too.
        // A selector cooldown still gates in part is withheld; the young locked versions cooldown
        // admits reach the native list as exact admissions instead.
        let NativeExemptions { exempt, withheld } = native_exemptions(
            &[PolicyContext {
                layers: &pctx.policy.layers,
                project: &pctx.rel_path,
            }],
            tool,
            self.now(),
        );
        let mut policy = ResolvedPolicy {
            admitted_versions: Some(Vec::new()),
            default_window: Some(window.clone()),
            exempt_packages: exempt,
            withheld_exemptions: withheld,
            package_windows: pctx.project.exclude_newer_package.clone(),
        };
        let result = async {
            let access = acquire_sync_access(writer, pctx, &self.lease_family(pctx), opts.dry_run)
                .await
                .map_err(|err| diag_from_error(&err, tool, &project, None))?;
            warnings.extend(access.recovery);
            if writer.requires_sync_admission_evaluation() {
                policy.admitted_versions = match self
                    .sync_admitted_versions(pctx, opts, &window, warnings)
                    .await
                {
                    Ok(admitted) => Some(admitted),
                    Err(warning) => {
                        if !warnings.contains(&warning) {
                            warnings.push(*warning);
                        }
                        None
                    }
                };
            }
            cooldown_core::interrupt::ensure_not_requested("writing native config")
                .map_err(|err| diag_from_error(&err, tool, &project, None))?;
            writer
                .write_native(&pctx.project, &policy, opts.dry_run)
                .await
                .map_err(|err| diag_from_error(&err, tool, &project, None))
        }
        .await;
        match result {
            Ok(report) => {
                let SyncClassification { status, path } = classify(&report, &pctx.project.root);
                summary.record(status);
                warnings.extend(report_warnings(report, tool, &project));
                SyncItem {
                    tool: tool.as_str().to_string(),
                    project,
                    status,
                    path,
                    window: Some(window_display(&window)),
                    error: None,
                }
            }
            Err(error) => {
                summary.errors += 1;
                SyncItem {
                    tool: tool.as_str().to_string(),
                    project,
                    status: SyncStatus::Error,
                    path: None,
                    window: None,
                    error: Some(error),
                }
            }
        }
    }

    /// Sync a tool's single repo-level native config ([`SyncScope::Repo`]), exactly once.
    ///
    /// No package-wide exemptions are written: discovery can exclude projects that still read the
    /// repo-level file, so no set of policy stacks proves an exemption for every consumer.
    async fn sync_repo(
        &self,
        writer: &dyn cooldown_core::ToolWrite,
        tool: ToolId,
        opts: &RunOpts,
        projects: &[&super::ProjectCtx],
        summary: &mut SyncSummary,
        warnings: &mut Vec<Diagnostic>,
    ) -> SyncItem {
        let project = repo_relative_root();
        // Resolve the repo-wide default window once against the repo-root cascade (no native layer),
        // independent of any single project's layers.
        // The `.` project keeps it to the bare default window — the only thing a single native
        // field can carry.
        let window =
            native_default_window(self.repo_layers(), tool, Utf8Path::new("."), self.now());
        let policy = ResolvedPolicy {
            admitted_versions: Some(Vec::new()),
            default_window: Some(window.clone()),
            exempt_packages: Vec::new(),
            withheld_exemptions: Vec::new(),
            package_windows: cooldown_core::package_windows(
                self.repo_layers(),
                tool,
                Utf8Path::new("."),
                self.now(),
            ),
        };
        let result =
            match acquire_repo_sync_access(writer, self, tool, projects, opts.dry_run, warnings)
                .await
            {
                Ok(_access) => {
                    writer
                        .write_repo_native(self.repo_root(), &policy, opts.dry_run)
                        .await
                }
                Err(error) => Err(error),
            };
        match result {
            Ok(report) => {
                let SyncClassification { status, path } = classify(&report, self.repo_root());
                summary.record(status);
                warnings.extend(report_warnings(report, tool, &project));
                SyncItem {
                    tool: tool.as_str().to_string(),
                    project,
                    status,
                    path,
                    window: Some(window_display(&window)),
                    error: None,
                }
            }
            Err(error) => {
                summary.errors += 1;
                let diagnostic = diag_from_error(&error, tool, &project, None);
                SyncItem {
                    tool: tool.as_str().to_string(),
                    project,
                    status: SyncStatus::Error,
                    path: None,
                    window: None,
                    error: Some(diagnostic),
                }
            }
        }
    }
}

/// The default window `sync` bakes into a tool's single native field for `project`.
///
/// This is the window cooldown gates a locked pin of an unnamed package with, so a package rule
/// that matches every name, such as `[package."*"]`, sets it just as the bare `min-age` does.
/// When that window is an exemption, the project default `min-age`, which package rules do not
/// change, is used instead: the native field still governs every package whose native exemption
/// [`native_exemptions`] withholds, so a package rule's `latest` or `allow` must not lift it.
/// The two windows are never compared by today's cutoffs, since a rolling age and an absolute
/// freeze trade places as time passes while the persisted value stays.
/// Per-kind and other per-package windows are not expressible in the single field.
fn native_default_window(
    layers: &[cooldown_core::PolicyLayer],
    tool: ToolId,
    project: &Utf8Path,
    now: jiff::Timestamp,
) -> WindowSpec {
    let window = |kind| {
        let query = ResolveQuery {
            tool,
            package: "",
            registry: None,
            project,
            kind,
        };
        resolve(layers, &query, now).window.effective_spec(now)
    };
    // The empty name stands for a package no rule names: only a glob matching the empty name,
    // such as `*`, applies to it.
    match window(ResolveKind::CurrentPin) {
        WindowSpec::Latest => window(ResolveKind::EffectiveDefault),
        unnamed => unnamed,
    }
}

/// Takes the project's lease of `family`, the tool's declared lease family, for one sync.
async fn acquire_sync_access(
    writer: &dyn ToolWrite,
    pctx: &super::ProjectCtx,
    family: &ManifestFamily,
    dry_run: bool,
) -> cooldown_core::Result<SyncAccess> {
    if dry_run {
        let guard = ProjectReadGuard::acquire(&pctx.project.root, family)?;
        writer.ensure_no_pending_mutation(&pctx.project).await?;
        Ok(SyncAccess {
            guard: SyncAccessGuard::Read { guard },
            recovery: Vec::new(),
        })
    } else {
        let guard = ProjectWriteGuard::acquire(&pctx.project.root, family)?;
        let recovery = writer
            .recover_pending_mutation(&pctx.project, guard.coordination())
            .await?;
        Ok(SyncAccess {
            guard: SyncAccessGuard::Write { guard },
            recovery: recovery_diagnostics(recovery, pctx.tool, pctx.rel_path.as_str()),
        })
    }
}

async fn acquire_repo_sync_access(
    writer: &dyn ToolWrite,
    ws: &Workspace,
    tool: ToolId,
    projects: &[&super::ProjectCtx],
    dry_run: bool,
    recovery: &mut Vec<Diagnostic>,
) -> cooldown_core::Result<RepoSyncAccess> {
    let resource = if dry_run {
        RepoSyncResourceGuard::Read {
            guard: RepoToolReadGuard::acquire(ws.repo_root(), tool)?,
        }
    } else {
        RepoSyncResourceGuard::Write {
            guard: RepoToolWriteGuard::acquire(ws.repo_root(), tool)?,
        }
    };
    let mut projects = projects.to_vec();
    projects.sort_by(|left, right| left.project.root.cmp(&right.project.root));
    projects.dedup_by(|left, right| left.project.root == right.project.root);
    let mut guards = Vec::with_capacity(projects.len());
    for project in projects {
        let access =
            acquire_sync_access(writer, project, &ws.lease_family(project), dry_run).await?;
        guards.push(access.guard);
        recovery.extend(access.recovery);
    }
    Ok(RepoSyncAccess {
        resource,
        projects: guards,
    })
}

/// The repo root as a repo-relative path: always `.`.
fn repo_relative_root() -> String {
    ".".to_string()
}

/// A [`SyncReport`] mapped to its report row parts.
struct SyncClassification {
    status: SyncStatus,
    /// The native config path the sync touched, relative to the row's base, when one exists.
    path: Option<String>,
}

/// Maps `report` to its row parts, spelling the touched path relative to `base`, the directory
/// the row's `project` names.
///
/// A path outside `base` has no relative spelling, so the row then reports none.
fn classify(report: &SyncReport, base: &Utf8Path) -> SyncClassification {
    let (status, path) = match report {
        SyncReport::Written { path, .. } => (SyncStatus::Written, Some(path)),
        SyncReport::Unchanged { path, .. } => (SyncStatus::Unchanged, Some(path)),
        SyncReport::Deferred { .. } => (SyncStatus::Written, None),
        SyncReport::Unsupported => (SyncStatus::Unsupported, None),
    };
    let path = path
        .and_then(|path| crate::report_path::relative_report_path(path, base))
        .map(Utf8PathBuf::into_string);
    SyncClassification { status, path }
}

/// The adapter's non-fatal diagnostics from `report`, attributed to the synced tool and project.
fn report_warnings(report: SyncReport, tool: ToolId, project: &str) -> Vec<Diagnostic> {
    let warnings = match report {
        SyncReport::Written { warnings, .. } | SyncReport::Unchanged { warnings, .. } => warnings,
        SyncReport::Deferred { .. } | SyncReport::Unsupported => Vec::new(),
    };
    warnings
        .into_iter()
        .map(|warning| warning.with_tool(tool.as_str()).with_project(project))
        .collect()
}

/// A short window label for display (`14d`, a freeze date, or `latest`).
fn window_display(spec: &WindowSpec) -> String {
    match spec {
        WindowSpec::MinAge(duration) => {
            format!("{}d", cooldown_core::duration::duration_as_days(*duration))
        }
        WindowSpec::Freeze(timestamp) => timestamp.to_string(),
        WindowSpec::Latest => "latest".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crate::app::workspace::tests::{TestAdapter, project_ctx};
    use crate::app::{
        AdapterSet, AdvisoryFailureMode, Baseline, Exit, Progress, RunOpts, Workspace,
    };
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use cooldown_core::ToolId;
    use cooldown_core::config::builtin_default_layer;
    use std::sync::Arc;

    const CARGO: ToolId = ToolId("cargo");

    /// A repository-scoped sync is one write for the tool's every project, so the display
    /// shows one live block for it and counts the other projects complete afterwards: one
    /// report row, one finished block per project, and the tool finished once.
    #[tokio::test]
    async fn a_repo_scoped_sync_counts_every_project_once() -> eyre::Result<()> {
        // Real directories, since the sync takes the projects' leases.
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir(root.join("nested"))?;
        let mut adapters = AdapterSet::new();
        adapters.register_target_verified_mutator(Arc::new(TestAdapter {
            write_id: CARGO,
            refresh: false,
            repo_sync: true,
        }))?;
        let ws = Workspace::new(
            adapters,
            vec![
                project_ctx(CARGO, root.as_str()),
                project_ctx(CARGO, root.join("nested").as_str()),
            ],
            "2026-06-17T00:00:00Z".parse()?,
            Baseline::default(),
            root.clone(),
            vec![builtin_default_layer()],
        );
        let progress = Progress::plain();
        progress.start_run(&[CARGO, CARGO]);
        let opts = RunOpts {
            progress: progress.clone(),
            ..RunOpts::default()
        };

        let out = ws.sync(&opts).await;

        assert_eq!(out.summary.errors, 0, "{:?}", out.warnings);
        assert_eq!(out.items.len(), 1);
        assert_eq!(progress.finished_blocks(), 2);
        assert_eq!(progress.completed_tools(), 1);
        Ok(())
    }

    struct AdmissionAdapter {
        deps: Vec<cooldown_core::Dependency>,
        published_at: Option<jiff::Timestamp>,
        fail_fetch: bool,
        /// The unattributed warnings every native sync reports.
        native_warnings: Vec<cooldown_core::Diagnostic>,
    }

    #[async_trait::async_trait]
    impl cooldown_core::ToolRead for AdmissionAdapter {
        fn id(&self) -> ToolId {
            CARGO
        }

        fn capabilities(&self) -> cooldown_core::Capabilities {
            cooldown_core::Capabilities {
                advisory_ecosystem: Some("crates.io"),
                ..cooldown_core::Capabilities::default()
            }
        }

        fn project_marker(&self) -> cooldown_core::ProjectMarker {
            cooldown_core::ProjectMarker {
                marker: "lock",
                manifest: "manifest",
                alternate_manifests: &[],
                workspace_root: true,
            }
        }

        async fn dependencies(
            &self,
            _project: &cooldown_core::Project,
            scope: cooldown_core::DepScope,
        ) -> cooldown_core::Result<Vec<cooldown_core::Dependency>> {
            Ok(self
                .deps
                .iter()
                .filter(|dep| scope == cooldown_core::DepScope::Graph || dep.direct)
                .cloned()
                .collect())
        }

        async fn native_policy(
            &self,
            _project: &cooldown_core::Project,
        ) -> cooldown_core::Result<Option<cooldown_core::NativePolicyLayer>> {
            Ok(None)
        }

        async fn verify_lock_current(
            &self,
            _project: &cooldown_core::Project,
        ) -> cooldown_core::Result<cooldown_core::LockVerifyReport> {
            Ok(cooldown_core::LockVerifyReport {
                status: cooldown_core::LockStatus::Current,
                detail: String::new(),
            })
        }
    }

    #[async_trait::async_trait]
    impl cooldown_core::ReleaseFetcher for AdmissionAdapter {
        async fn releases(
            &self,
            _dep: &cooldown_core::Dependency,
            _fetch: &cooldown_core::FetchContext<'_>,
            _candidates: cooldown_core::CandidateScope,
        ) -> cooldown_core::Result<Vec<cooldown_core::Release>> {
            Ok(Vec::new())
        }

        async fn locked_release(
            &self,
            dep: &cooldown_core::Dependency,
            _fetch: &cooldown_core::FetchContext<'_>,
        ) -> cooldown_core::Result<cooldown_core::Release> {
            if self.fail_fetch {
                return Err(cooldown_core::CoreError::NotFound(dep.package.name.clone()));
            }
            Ok(cooldown_core::Release {
                version: dep.current.clone(),
                order: cooldown_core::ReleaseOrder(vec![1]),
                major: cooldown_core::MajorKey("1".to_string()),
                major_number: Some(1),
                kind_from_current: None,
                beyond_declared_bound: false,
                beyond_latest_tag: false,
                published_at: self.published_at,
                yanked: false,
                quality: dep.current_quality,
            })
        }
    }

    #[async_trait::async_trait]
    impl cooldown_core::ToolWrite for AdmissionAdapter {
        fn mutation_tool(&self) -> ToolId {
            CARGO
        }

        async fn mutation_journal(
            &self,
            project: &cooldown_core::Project,
            _plan: &cooldown_core::Plan,
        ) -> cooldown_core::Result<cooldown_core::ProjectMutationJournal> {
            cooldown_core::ProjectMutationJournal::capture(
                &project.root,
                std::iter::empty::<&camino::Utf8Path>(),
            )
        }

        async fn apply(
            &self,
            mutation: &cooldown_core::PreparedMutation,
        ) -> cooldown_core::Result<cooldown_core::ApplyReport> {
            mutation.parts_for(self)?;
            Ok(cooldown_core::ApplyReport::default())
        }

        async fn build(
            &self,
            _project: &cooldown_core::Project,
        ) -> cooldown_core::Result<cooldown_core::VerifyReport> {
            Ok(cooldown_core::VerifyReport {
                ok: true,
                detail: String::new(),
            })
        }

        fn sync_scope(&self) -> cooldown_core::SyncScope {
            cooldown_core::SyncScope::Project
        }

        fn requires_sync_admission_evaluation(&self) -> bool {
            true
        }

        async fn write_native(
            &self,
            project: &cooldown_core::Project,
            policy: &cooldown_core::ResolvedPolicy,
            dry_run: bool,
        ) -> cooldown_core::Result<cooldown_core::SyncReport> {
            let path = project.root.join("native-exclusions");
            let exclusions = if let Some(admitted) = &policy.admitted_versions {
                admitted
                    .iter()
                    .map(|admission| format!("{}@{}", admission.name, admission.version))
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                std::fs::read_to_string(&path).map_err(|err| {
                    cooldown_core::CoreError::Filesystem(format!("failed to read {path}: {err}"))
                })?
            };
            if !dry_run {
                std::fs::write(&path, exclusions).map_err(|err| {
                    cooldown_core::CoreError::Filesystem(format!("failed to write {path}: {err}"))
                })?;
                let selectors_path = project.root.join("native-selectors");
                let selectors = indoc::formatdoc! {"
                    exempt: [{exempt}]
                    withheld: [{withheld}]
                ",
                    exempt = policy.exempt_packages.join(","),
                    withheld = policy.withheld_exemptions.join(","),
                };
                std::fs::write(&selectors_path, selectors).map_err(|err| {
                    cooldown_core::CoreError::Filesystem(format!(
                        "failed to write {selectors_path}: {err}"
                    ))
                })?;
            }
            Ok(cooldown_core::SyncReport::Written {
                path,
                warnings: self.native_warnings.clone(),
            })
        }
    }

    fn admission_dependency(name: &str, direct: bool) -> cooldown_core::Dependency {
        cooldown_core::Dependency {
            package: cooldown_core::PackageId::new(CARGO, name, None),
            advisory_identity: None,
            current: cooldown_core::Version::new("1.2.3"),
            current_quality: cooldown_core::ReleaseQuality::Stable,
            direct,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    fn admission_workspace(
        root: &camino::Utf8Path,
        adapter: AdmissionAdapter,
        now: jiff::Timestamp,
        baseline: Baseline,
    ) -> eyre::Result<Workspace> {
        let mut adapters = AdapterSet::new();
        adapters.register_target_verified_mutator(Arc::new(adapter))?;
        Ok(Workspace::new(
            adapters,
            vec![project_ctx(CARGO, root.as_str())],
            now,
            baseline,
            root.to_owned(),
            vec![builtin_default_layer()],
        ))
    }

    fn acknowledged(package: &str) -> crate::app::baseline::AckEntry {
        crate::app::baseline::AckEntry {
            tool: CARGO.as_str().to_string(),
            project: ".".to_string(),
            package: package.to_string(),
            version: "1.2.3".to_string(),
            registry: None,
            published_at: None,
            window_days: None,
            reason: None,
            until: None,
        }
    }

    /// Native admissions are recomputed from the locked graph: retained and newly acknowledged
    /// pins survive, removed pins disappear, and every exact admission disappears after maturity.
    #[tokio::test]
    async fn sync_recomputes_exact_admissions_and_prunes_mature_versions() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("native-exclusions");
        std::fs::write(&path, "retained@1.2.3,removed@1.2.3")?;
        let published_at = Some("2026-06-16T00:00:00Z".parse()?);
        let baseline = Baseline {
            entries: vec![acknowledged("retained"), acknowledged("added")],
        };
        let deps = vec![
            admission_dependency("retained", true),
            admission_dependency("added", true),
        ];
        let ws = admission_workspace(
            &root,
            AdmissionAdapter {
                deps: deps.clone(),
                published_at,
                fail_fetch: false,
                native_warnings: Vec::new(),
            },
            "2026-06-17T00:00:00Z".parse()?,
            baseline.clone(),
        )?;
        let outcome = ws.sync(&RunOpts::default()).await;
        assert_eq!(outcome.summary.errors, 0);
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "added@1.2.3,retained@1.2.3"
        );

        let mature = admission_workspace(
            &root,
            AdmissionAdapter {
                deps,
                published_at,
                fail_fetch: false,
                native_warnings: Vec::new(),
            },
            "2026-07-17T00:00:00Z".parse()?,
            baseline,
        )?;
        assert_eq!(mature.sync(&RunOpts::default()).await.summary.errors, 0);
        assert_eq!(std::fs::read_to_string(&path)?, "");
        Ok(())
    }

    /// An adapter's native sync warnings reach the outcome attributed to the synced tool and
    /// project, in a dry run as in a real one, beside the written item.
    #[tokio::test]
    async fn sync_attributes_native_warnings_to_the_tool_and_project() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let warning = cooldown_core::Diagnostic::new(
            cooldown_core::DiagnosticKind::StricterNative,
            "selector `a?` is left out",
        );
        let ws = admission_workspace(
            &root,
            AdmissionAdapter {
                deps: Vec::new(),
                published_at: None,
                fail_fetch: false,
                native_warnings: vec![warning.clone()],
            },
            "2026-06-17T00:00:00Z".parse()?,
            Baseline::default(),
        )?;
        let expected = warning.with_tool(CARGO.as_str()).with_project(".");
        for dry_run in [true, false] {
            let opts = RunOpts {
                dry_run,
                ..RunOpts::default()
            };
            let outcome = ws.sync(&opts).await;
            assert_eq!(outcome.summary.written, 1, "dry run: {dry_run}");
            assert_eq!(
                outcome.warnings,
                std::slice::from_ref(&expected),
                "dry run: {dry_run}"
            );
            assert_eq!(outcome.exit, Exit::Ok);
        }
        Ok(())
    }

    /// Only a written or unchanged report carries native warnings, and each is attributed.
    #[test]
    fn report_warnings_attribute_written_and_unchanged_reports() {
        let warning = cooldown_core::Diagnostic::new(
            cooldown_core::DiagnosticKind::StricterNative,
            "selector `a?` is left out",
        );
        let expected = [warning.clone().with_tool("uv").with_project(".")];
        let path = Utf8PathBuf::from("uv.toml");
        let written = cooldown_core::SyncReport::Written {
            path: path.clone(),
            warnings: vec![warning.clone()],
        };
        let unchanged = cooldown_core::SyncReport::Unchanged {
            path,
            warnings: vec![warning],
        };
        assert_eq!(super::report_warnings(written, ToolId("uv"), "."), expected);
        assert_eq!(
            super::report_warnings(unchanged, ToolId("uv"), "."),
            expected
        );
        assert_eq!(
            super::report_warnings(cooldown_core::SyncReport::Unsupported, ToolId("uv"), "."),
            []
        );
    }

    /// A registry lookup failure warns while preserving the exact native exclusions.
    #[tokio::test]
    async fn sync_evaluation_failure_preserves_native_exclusions() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let path = root.join("native-exclusions");
        std::fs::write(&path, "retained@1.2.3")?;
        let ws = admission_workspace(
            &root,
            AdmissionAdapter {
                deps: vec![admission_dependency("retained", true)],
                published_at: None,
                fail_fetch: true,
                native_warnings: Vec::new(),
            },
            "2026-06-17T00:00:00Z".parse()?,
            Baseline::default(),
        )?;
        let outcome = ws.sync(&RunOpts::default()).await;
        assert_eq!(outcome.summary.errors, 0);
        assert_eq!(outcome.summary.written, 1);
        assert_ne!(outcome.warnings, []);
        assert!(outcome.items.iter().all(|item| item.error.is_none()));
        assert_eq!(outcome.exit, Exit::Ok);
        assert_eq!(std::fs::read_to_string(path)?, "retained@1.2.3");
        Ok(())
    }

    /// Tolerating a transitive violation does not exempt it from the native age gate.
    #[tokio::test]
    async fn sync_admits_only_allowed_transitive_pins() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let ws = admission_workspace(
            &root,
            AdmissionAdapter {
                deps: vec![
                    admission_dependency("direct", true),
                    admission_dependency("transitive", false),
                ],
                published_at: Some("2026-06-16T00:00:00Z".parse()?),
                fail_fetch: false,
                native_warnings: Vec::new(),
            },
            "2026-06-17T00:00:00Z".parse()?,
            Baseline::default(),
        )?;
        let opts = RunOpts {
            transitive_mode: crate::app::TransitiveGate::Allow,
            ..RunOpts::default()
        };
        assert_eq!(ws.sync(&opts).await.summary.errors, 0);
        assert_eq!(std::fs::read_to_string(root.join("native-exclusions"))?, "");
        assert_eq!(ws.sync(&RunOpts::default()).await.summary.errors, 0);
        assert_eq!(std::fs::read_to_string(root.join("native-exclusions"))?, "");
        Ok(())
    }

    /// A hidden transitive pin admitted by a shorter package window still needs an exact native
    /// exemption, while a hidden transitive violation stays subject to the native default.
    #[tokio::test]
    async fn sync_evaluates_hidden_transitive_pins_against_package_policy() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let adapter = AdmissionAdapter {
            deps: vec![
                admission_dependency("shortened", false),
                admission_dependency("held", false),
            ],
            published_at: Some("2026-06-14T00:00:00Z".parse()?),
            fail_fetch: false,
            native_warnings: Vec::new(),
        };
        let mut adapters = AdapterSet::new();
        adapters.register_target_verified_mutator(Arc::new(adapter))?;
        let mut project = project_ctx(CARGO, root.as_str());
        let mut layer = cooldown_core::PolicyLayer::new(cooldown_core::Origin::Cli);
        let mut rule = cooldown_core::Rule::new(cooldown_core::Selector::Package {
            glob: cooldown_core::PatternGlob::new("shortened")?,
            tool: Some(CARGO),
        });
        rule.window = cooldown_core::ByKind::scalar(cooldown_core::WindowSpec::MinAge(
            jiff::SignedDuration::from_hours(24),
        ));
        layer.rules.push(rule);
        project.policy.layers.push(layer);
        let ws = Workspace::new(
            adapters,
            vec![project],
            "2026-06-17T00:00:00Z".parse()?,
            Baseline::default(),
            root.clone(),
            vec![builtin_default_layer()],
        );
        let opts = RunOpts {
            transitive_mode: crate::app::TransitiveGate::Hide,
            ..RunOpts::default()
        };
        assert_eq!(ws.sync(&opts).await.summary.errors, 0);
        assert_eq!(
            std::fs::read_to_string(root.join("native-exclusions"))?,
            "shortened@1.2.3"
        );
        Ok(())
    }

    enum AdmissionFeed {
        Unavailable,
        Stale,
    }

    #[async_trait::async_trait]
    impl cooldown_core::AdvisorySource for AdmissionFeed {
        fn id(&self) -> cooldown_core::AdvisorySourceId {
            cooldown_core::AdvisorySourceId("osv")
        }

        async fn advisories(
            &self,
            _ecosystem: &str,
            packages: &[String],
        ) -> cooldown_core::Result<cooldown_core::AdvisoryFetch> {
            match self {
                Self::Unavailable => Err(cooldown_core::CoreError::System(
                    "feed unavailable".to_string(),
                )),
                Self::Stale => Ok(cooldown_core::AdvisoryFetch {
                    packages: packages
                        .iter()
                        .map(|package| cooldown_core::PackageAdvisories {
                            package: package.clone(),
                            advisories: Vec::new(),
                        })
                        .collect(),
                    stale: true,
                }),
            }
        }
    }

    /// Unavailable or stale advisory evidence cannot authorize pruning native security-fix
    /// admissions, even when ordinary check would surface the feed failure as a warning.
    #[tokio::test]
    async fn sync_unusable_advisory_evidence_preserves_native_exclusions() -> eyre::Result<()> {
        for (feed, failure_mode) in [
            (AdmissionFeed::Unavailable, AdvisoryFailureMode::Warn),
            (AdmissionFeed::Stale, AdvisoryFailureMode::Warn),
            (AdmissionFeed::Unavailable, AdvisoryFailureMode::Error),
            (AdmissionFeed::Stale, AdvisoryFailureMode::Error),
        ] {
            let directory = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
                .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
            let path = root.join("native-exclusions");
            let before = b"retained@1.2.3";
            std::fs::write(&path, before)?;
            let mut dep = admission_dependency("retained", true);
            dep.advisory_identity = Some("retained".to_string());
            let adapter = AdmissionAdapter {
                deps: vec![dep],
                published_at: Some("2026-06-16T00:00:00Z".parse()?),
                fail_fetch: false,
                native_warnings: Vec::new(),
            };
            let mut adapters = AdapterSet::new();
            adapters.register_target_verified_mutator(Arc::new(adapter))?;
            let mut project = project_ctx(CARGO, root.as_str());
            let mut layer = cooldown_core::PolicyLayer::new(cooldown_core::Origin::Cli);
            layer.advisories = Some(cooldown_core::AdvisoryPolicy {
                enabled: Some(true),
                mode: Some(cooldown_core::AdvisoryMode::Shorten),
                ..cooldown_core::AdvisoryPolicy::default()
            });
            project.policy.layers.push(layer);
            let ws = Workspace::new(
                adapters,
                vec![project],
                "2026-06-17T00:00:00Z".parse()?,
                Baseline::default(),
                root,
                vec![builtin_default_layer()],
            )
            .with_advisory_source(Arc::new(feed));
            let opts = RunOpts {
                advisory_failure: failure_mode,
                ..RunOpts::default()
            };
            let outcome = ws.sync(&opts).await;
            assert_eq!(outcome.summary.errors, 0);
            assert_eq!(outcome.summary.written, 1);
            assert_eq!(std::fs::read(path)?, before);
            assert!(outcome.items.iter().all(|item| item.error.is_none()));
            assert_eq!(outcome.exit, Exit::Ok);
            let warning = outcome
                .warnings
                .iter()
                .find(|warning| {
                    warning.kind == cooldown_core::DiagnosticKind::AdvisorySourceUnavailable
                })
                .ok_or_else(|| eyre::eyre!("sync did not warn about advisory failure"))?;
            assert_eq!(
                warning.kind,
                cooldown_core::DiagnosticKind::AdvisorySourceUnavailable
            );
        }
        Ok(())
    }

    /// A baseline acknowledgement cannot produce an exact native admission without a known
    /// publish time, because sync cannot prove the locked version needs the native exemption.
    #[tokio::test]
    async fn sync_omits_unknown_age_versions() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let ws = admission_workspace(
            &root,
            AdmissionAdapter {
                deps: vec![admission_dependency("unknown", true)],
                published_at: None,
                fail_fetch: false,
                native_warnings: Vec::new(),
            },
            "2026-06-17T00:00:00Z".parse()?,
            Baseline {
                entries: vec![acknowledged("unknown")],
            },
        )?;
        let outcome = ws.sync(&RunOpts::default()).await;
        assert_eq!(outcome.summary.errors, 0);
        assert_eq!(std::fs::read_to_string(root.join("native-exclusions"))?, "");
        Ok(())
    }

    /// A per-kind window that holds majors to 30 days, as `[min-age]` writes it.
    const PER_KIND_WINDOW: &str = indoc::indoc! {r#"
        [min-age]
        default = "7d"
        major = "30d"
    "#};

    /// A `latest` exemption for every `@corp/` package.
    const CORP_LATEST: &str = indoc::indoc! {r#"
        [package."@corp/*"]
        latest = true
    "#};

    /// [`PER_KIND_WINDOW`] and [`CORP_LATEST`] in one document.
    const PER_KIND_WINDOW_AND_CORP_LATEST: &str = indoc::indoc! {r#"
        [min-age]
        default = "7d"
        major = "30d"

        [package."@corp/*"]
        latest = true
    "#};

    fn repo_config(content: &str) -> eyre::Result<cooldown_core::PolicyLayer> {
        Ok(cooldown_core::config::parse_config(
            content,
            cooldown_core::Origin::Repo("cooldown.toml".into()),
        )?)
    }

    /// A workspace whose one project resolves `layers`, lowest authority first, above the
    /// built-in default.
    fn policy_workspace(
        root: &camino::Utf8Path,
        adapter: AdmissionAdapter,
        baseline: Baseline,
        layers: Vec<cooldown_core::PolicyLayer>,
    ) -> eyre::Result<Workspace> {
        let mut adapters = AdapterSet::new();
        adapters.register_target_verified_mutator(Arc::new(adapter))?;
        let mut project = project_ctx(CARGO, root.as_str());
        project.policy.layers.extend(layers);
        Ok(Workspace::new(
            adapters,
            vec![project],
            "2026-06-17T00:00:00Z".parse()?,
            baseline,
            root.to_owned(),
            vec![builtin_default_layer()],
        ))
    }

    /// An adapter whose locked `deps` were all published at `published_at`.
    fn locked_adapter(
        deps: Vec<cooldown_core::Dependency>,
        published_at: Option<&str>,
    ) -> eyre::Result<AdmissionAdapter> {
        Ok(AdmissionAdapter {
            deps,
            published_at: published_at.map(str::parse).transpose()?,
            fail_fetch: false,
            native_warnings: Vec::new(),
        })
    }

    /// The `latest` glob of the issue's policy exempts every kind of `@corp/*` but majors, so
    /// sync withholds it from the native list whether the per-kind window shares its document or
    /// sits in a lower-authority one.
    /// The young pins cooldown passes are admitted exactly instead: `@corp/ui` through its `latest`
    /// default and `other` through a baseline acknowledgement.
    #[tokio::test]
    async fn sync_withholds_a_partially_exempt_glob() -> eyre::Result<()> {
        let arrangements = [
            vec![repo_config(PER_KIND_WINDOW_AND_CORP_LATEST)?],
            vec![
                cooldown_core::config::parse_config(
                    PER_KIND_WINDOW,
                    cooldown_core::Origin::Global,
                )?,
                repo_config(CORP_LATEST)?,
            ],
        ];
        for layers in arrangements {
            let directory = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
                .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
            let ws = policy_workspace(
                &root,
                locked_adapter(
                    vec![
                        admission_dependency("@corp/ui", true),
                        admission_dependency("other", true),
                    ],
                    Some("2026-06-16T00:00:00Z"),
                )?,
                Baseline {
                    entries: vec![acknowledged("other")],
                },
                layers,
            )?;

            let outcome = ws.sync(&RunOpts::default()).await;

            assert_eq!(outcome.summary.errors, 0, "{:?}", outcome.warnings);
            assert_eq!(
                std::fs::read_to_string(root.join("native-selectors"))?,
                indoc::indoc! {"
                    exempt: []
                    withheld: [@corp/*]
                "}
            );
            assert_eq!(
                std::fs::read_to_string(root.join("native-exclusions"))?,
                "@corp/ui@1.2.3,other@1.2.3"
            );
        }
        Ok(())
    }

    /// Without the per-kind window every release of `@corp/*` is exempt, so the glob stays a
    /// package-wide native exemption.
    #[tokio::test]
    async fn sync_keeps_a_fully_exempt_glob() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let ws = policy_workspace(
            &root,
            locked_adapter(Vec::new(), None)?,
            Baseline::default(),
            vec![repo_config(CORP_LATEST)?],
        )?;

        assert_eq!(ws.sync(&RunOpts::default()).await.summary.errors, 0);
        assert_eq!(
            std::fs::read_to_string(root.join("native-selectors"))?,
            indoc::indoc! {"
                exempt: [@corp/*]
                withheld: []
            "}
        );
        Ok(())
    }

    /// A lower layer's floor on `@corp/sensitive` survives the repository's `allow = ["@corp/*"]`,
    /// so the glob is withheld.
    /// The young `@corp/ui` the allow exempts gets an exact admission instead, while the floored
    /// `@corp/sensitive` stays gated.
    #[tokio::test]
    async fn sync_admits_exempt_pins_of_a_withheld_allow() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let floor = cooldown_core::config::parse_config(
            indoc::indoc! {r#"
                [package."@corp/sensitive"]
                floor = "30d"
            "#},
            cooldown_core::Origin::Global,
        )?;
        let ws = policy_workspace(
            &root,
            locked_adapter(
                vec![
                    admission_dependency("@corp/ui", true),
                    admission_dependency("@corp/sensitive", true),
                ],
                Some("2026-06-16T00:00:00Z"),
            )?,
            Baseline::default(),
            vec![floor, repo_config(r#"allow = ["@corp/*"]"#)?],
        )?;

        let outcome = ws.sync(&RunOpts::default()).await;

        assert_eq!(outcome.summary.errors, 0, "{:?}", outcome.warnings);
        assert_eq!(
            std::fs::read_to_string(root.join("native-selectors"))?,
            indoc::indoc! {"
                exempt: []
                withheld: [@corp/*]
            "}
        );
        assert_eq!(
            std::fs::read_to_string(root.join("native-exclusions"))?,
            "@corp/ui@1.2.3"
        );
        Ok(())
    }

    /// An exempt pin earns an exact admission only as a registry version of known age that is
    /// still younger than the native window: a pseudo-version, an unknown-age pin and a mature
    /// pin get none.
    #[tokio::test]
    async fn sync_admits_only_young_known_age_exempt_registry_pins() -> eyre::Result<()> {
        let mut pseudo = admission_dependency("@corp/pseudo", true);
        pseudo.current_quality = cooldown_core::ReleaseQuality::Pseudo;
        let cases = [
            (pseudo, Some("2026-06-16T00:00:00Z")),
            (admission_dependency("@corp/unknown", true), None),
            (
                admission_dependency("@corp/mature", true),
                Some("2026-05-01T00:00:00Z"),
            ),
        ];
        for (dep, published_at) in cases {
            let directory = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
                .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
            let name = dep.package.name.clone();
            let ws = policy_workspace(
                &root,
                locked_adapter(vec![dep], published_at)?,
                Baseline::default(),
                vec![repo_config(r#"allow = ["@corp/*"]"#)?],
            )?;

            let outcome = ws.sync(&RunOpts::default()).await;

            assert_eq!(outcome.summary.errors, 0, "{name}: {:?}", outcome.warnings);
            assert_eq!(
                std::fs::read_to_string(root.join("native-exclusions"))?,
                "",
                "{name}"
            );
        }
        Ok(())
    }

    /// A `[package."*"]` rule sets the native default like the bare `min-age`, shorter or longer,
    /// yet its exemption does not turn the native default into an opt-out: the native window still
    /// governs every package whose exemption sync withholds.
    /// A catch-all freeze stays a freeze although a rolling 7 days is stricter today, since uv and
    /// pnpm would otherwise admit post-freeze releases once 7 days pass.
    #[tokio::test]
    async fn sync_bakes_the_unnamed_package_window_unless_it_is_exempt() -> eyre::Result<()> {
        let cases = [
            (
                indoc::indoc! {r#"
                    [package."*"]
                    latest = true
                "#},
                "7d",
            ),
            (
                indoc::indoc! {r#"
                    min-age = "14d"
                    allow = ["*"]
                "#},
                "14d",
            ),
            (
                indoc::indoc! {r#"
                    min-age = "14d"

                    [package."*"]
                    min-age = "30d"
                "#},
                "30d",
            ),
            (
                indoc::indoc! {r#"
                    min-age = "14d"

                    [package."*"]
                    min-age = "3d"
                "#},
                "3d",
            ),
            (
                indoc::indoc! {r#"
                    min-age = "7d"

                    [package."*"]
                    freeze = "2026-06-15T00:00:00Z"
                "#},
                "2026-06-15T00:00:00Z",
            ),
        ];
        for (config, expected) in cases {
            let directory = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
                .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
            let ws = policy_workspace(
                &root,
                locked_adapter(Vec::new(), None)?,
                Baseline::default(),
                vec![repo_config(config)?],
            )?;

            let outcome = ws.sync(&RunOpts::default()).await;

            assert_eq!(
                outcome.summary.errors, 0,
                "{config}: {:?}",
                outcome.warnings
            );
            let windows: Vec<_> = outcome
                .items
                .iter()
                .map(|item| item.window.as_deref())
                .collect();
            assert_eq!(windows, [Some(expected)], "{config}");
        }
        Ok(())
    }

    /// The review's scenario through the real pnpm adapter: an earlier sync left `["*"]` in
    /// `minimumReleaseAgeExclude`, and the policy now exempts `*` from every kind but majors.
    /// sync keeps the 7-day native window, withholds `*` with a warning, and replaces the broad
    /// entry with an explicit empty list, so pnpm no longer admits a brand-new major.
    #[tokio::test]
    async fn sync_replaces_a_withheld_catch_all_in_pnpm_config() -> eyre::Result<()> {
        const PNPM: ToolId = ToolId("pnpm");
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("package.json"),
            indoc::indoc! {r#"
                {
                  "name": "cooldown-sync-fixture",
                  "version": "0.1.0",
                  "private": true
                }
            "#},
        )?;
        std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n")?;
        let workspace_yaml = root.join("pnpm-workspace.yaml");
        std::fs::write(
            &workspace_yaml,
            indoc::indoc! {r#"
                packages: []
                minimumReleaseAge: 10080
                minimumReleaseAgeExclude:
                  - "*"
            "#},
        )?;
        let cache = tempfile::tempdir()?;
        let tool = cooldown_npm::PnpmTool::from_http(cooldown_registry::SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
        let mut adapters = AdapterSet::new();
        adapters.register_target_verified_mutator(Arc::new(tool))?;
        let mut project = project_ctx(PNPM, root.as_str());
        project.project.manifest = root.join("package.json");
        project.policy.layers.push(repo_config(indoc::indoc! {r#"
            [min-age]
            default = "7d"
            major = "30d"

            [package."*"]
            latest = true
        "#})?);
        let ws = Workspace::new(
            adapters,
            vec![project],
            "2026-06-17T00:00:00Z".parse()?,
            Baseline::default(),
            root.clone(),
            vec![builtin_default_layer()],
        );

        let outcome = ws.sync(&RunOpts::default()).await;

        assert_eq!(outcome.summary.errors, 0, "{:?}", outcome.items);
        assert_eq!(outcome.summary.written, 1, "{:?}", outcome.items);
        assert!(
            outcome.warnings.iter().any(|warning| warning
                .message
                .contains("cannot prove that every release `*` matches is exempt")),
            "{:?}",
            outcome.warnings
        );
        assert_eq!(
            std::fs::read_to_string(&workspace_yaml)?,
            indoc::indoc! {"
                packages: []
                minimumReleaseAge: 10080
                minimumReleaseAgeExclude: []
            "}
        );

        // A second sync finds nothing left to change.
        assert_eq!(ws.sync(&RunOpts::default()).await.summary.unchanged, 1);
        Ok(())
    }
}
