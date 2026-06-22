//! `sync` — write the resolved cooldown policy into each project's native config (uv
//! `exclude-newer`, …), so `cooldown.toml` is the single source of truth and even a bare `uv sync`
//! someone runs by hand still respects the window. Tools without a native cooldown concept (Go,
//! Cargo) report `unsupported`; nothing is written for them.

use super::{Exit, RunOpts, Workspace, diag_from_error};
use camino::Utf8Path;
use cooldown_core::{
    Diagnostic, ResolveKind, ResolveQuery, ResolvedPolicy, SyncReport, SyncScope, ToolId,
    WindowSpec, resolve,
};

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
    /// The native config file written or checked, when applicable.
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
    /// Tally one non-error outcome. An [`SyncStatus::Error`] is counted at the failing call site
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
    /// Project-level errors (none today; per-project failures live on their [`SyncItem`]).
    pub errors: Vec<Diagnostic>,
    /// The process exit: non-zero if any project failed to sync.
    pub exit: Exit,
}

impl Workspace {
    /// Write the resolved cooldown policy down into native config, dispatching on each tool's
    /// [`SyncScope`].
    ///
    /// A [`SyncScope::Project`] tool is synced per in-scope project (its manifest's native field).
    /// A [`SyncScope::Repo`] tool's single repo-level file (uv's root `uv.toml`) is resolved against
    /// the repo-wide cascade and written **exactly once per tool**, no matter how many of its
    /// projects are in scope — so concurrent project upgrades never race on the shared file. A
    /// [`SyncScope::None`] tool reports a single `unsupported` item.
    ///
    /// Idempotent: a target already in sync is reported `unchanged` and not rewritten. Fail-soft: a
    /// write failure becomes one `error` item (and a non-zero exit) without aborting the rest.
    pub async fn sync(&self, opts: &RunOpts) -> SyncOutcome {
        let mut items = Vec::new();
        let mut summary = SyncSummary::default();

        // The distinct in-scope tools, in first-seen order. Each is handled once: a repo-scoped tool
        // is written exactly once (never per project), a project-scoped tool iterates its own
        // projects. The final item list is sorted below, so this order is not load-bearing.
        let mut tools: Vec<ToolId> = Vec::new();
        for pctx in self.scoped_projects(opts) {
            if !tools.contains(&pctx.tool) {
                tools.push(pctx.tool);
            }
        }

        for tool in tools {
            let Some(writer) = self.mutator(tool) else {
                summary.unsupported += 1;
                items.push(SyncItem {
                    tool: tool.as_str().to_string(),
                    project: repo_relative_root(),
                    status: SyncStatus::Unsupported,
                    path: None,
                    window: None,
                    error: None,
                });
                continue;
            };

            match writer.sync_scope() {
                SyncScope::Project => {
                    for pctx in self.scoped_projects(opts).filter(|pctx| pctx.tool == tool) {
                        items.push(
                            self.sync_project(writer, pctx, tool, opts, &mut summary)
                                .await,
                        );
                    }
                }
                SyncScope::Repo => {
                    items.push(self.sync_repo(writer, tool, opts, &mut summary).await);
                }
                SyncScope::None => {
                    summary.unsupported += 1;
                    items.push(SyncItem {
                        tool: tool.as_str().to_string(),
                        project: repo_relative_root(),
                        status: SyncStatus::Unsupported,
                        path: None,
                        window: None,
                        error: None,
                    });
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
            errors: Vec::new(),
            exit,
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
    ) -> SyncItem {
        let project = pctx.rel_path.to_string();
        // Resolve the policy's default (bare) window for this project. The empty package name
        // matches no package-specific rule, so this is the window `sync` bakes into the single
        // native field; per-package and per-kind windows are not expressible there.
        let query = ResolveQuery {
            tool,
            package: "",
            registry: None,
            project: &pctx.rel_path,
            kind: ResolveKind::CurrentPin,
        };
        let resolved = resolve(&pctx.policy.layers, &query, self.now());
        let window = resolved.window.effective_spec(self.now());
        let policy = ResolvedPolicy {
            default_window: Some(window.clone()),
        };
        match writer
            .write_native(&pctx.project, &policy, opts.dry_run)
            .await
        {
            Ok(report) => {
                let (status, path) = classify(&report);
                summary.record(status);
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

    /// Sync a tool's single repo-level native config ([`SyncScope::Repo`]), exactly once.
    async fn sync_repo(
        &self,
        writer: &dyn cooldown_core::ToolWrite,
        tool: ToolId,
        opts: &RunOpts,
        summary: &mut SyncSummary,
    ) -> SyncItem {
        let project = repo_relative_root();
        // Resolve the repo-wide default window once against the repo-root cascade (no native layer),
        // independent of any single project's layers. The empty package name and `.` project keep it
        // to the bare default window — the only thing a single native field can carry.
        let query = ResolveQuery {
            tool,
            package: "",
            registry: None,
            project: Utf8Path::new("."),
            kind: ResolveKind::CurrentPin,
        };
        let resolved = resolve(self.repo_layers(), &query, self.now());
        let window = resolved.window.effective_spec(self.now());
        let policy = ResolvedPolicy {
            default_window: Some(window.clone()),
        };
        match writer
            .write_repo_native(self.repo_root(), &policy, opts.dry_run)
            .await
        {
            Ok(report) => {
                let (status, path) = classify(&report);
                summary.record(status);
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

/// The repo root as a repo-relative path: always `.`.
fn repo_relative_root() -> String {
    ".".to_string()
}

/// Map a [`SyncReport`] to a [`SyncStatus`] and the native config path it touched.
fn classify(report: &SyncReport) -> (SyncStatus, Option<String>) {
    match report {
        SyncReport::Written { path } => (SyncStatus::Written, Some(path.to_string())),
        SyncReport::Unchanged { path } => (SyncStatus::Unchanged, Some(path.to_string())),
        SyncReport::Deferred { .. } => (SyncStatus::Written, None),
        SyncReport::Unsupported => (SyncStatus::Unsupported, None),
    }
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
