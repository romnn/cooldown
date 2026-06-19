//! `sync` — write the resolved cooldown policy into each project's native config (uv
//! `exclude-newer`, …), so `cooldown.toml` is the single source of truth and even a bare `uv sync`
//! someone runs by hand still respects the window. Tools without a native cooldown concept (Go,
//! Cargo) report `unsupported`; nothing is written for them.

use super::{Exit, RunOpts, Workspace, diag_from_error};
use cooldown_core::{
    Diagnostic, ResolveKind, ResolveQuery, ResolvedPolicy, SyncReport, WindowSpec, resolve,
};
use jiff::SignedDuration;

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
    /// Write the resolved policy's default window into each in-scope project's native config.
    ///
    /// Idempotent: a project already in sync is reported `unchanged` and its manifest is not
    /// rewritten. Fail-soft per project: a write failure becomes an `error` item (and a non-zero
    /// exit) without aborting the other projects.
    pub async fn sync(&self, opts: &RunOpts) -> SyncOutcome {
        let mut items = Vec::new();
        let mut summary = SyncSummary::default();

        for pctx in self.scoped_projects(opts) {
            let project = pctx.rel_path.to_string();
            let tool = pctx.tool;
            let Some(writer) = self.mutator(tool) else {
                summary.unsupported += 1;
                items.push(SyncItem {
                    tool: tool.as_str().to_string(),
                    project,
                    status: SyncStatus::Unsupported,
                    path: None,
                    window: None,
                    error: None,
                });
                continue;
            };

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
            let window = effective_window(&resolved.window.spec, resolved.window.floor);
            let policy = ResolvedPolicy {
                default_window: Some(window.clone()),
            };

            let item = match writer
                .write_native(&pctx.project, &policy, opts.dry_run)
                .await
            {
                Ok(report) => {
                    let (status, path) = classify(&report);
                    match status {
                        SyncStatus::Written => summary.written += 1,
                        SyncStatus::Unchanged => summary.unchanged += 1,
                        SyncStatus::Unsupported => summary.unsupported += 1,
                        SyncStatus::Error => {}
                    }
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
                        project: project.clone(),
                        status: SyncStatus::Error,
                        path: None,
                        window: None,
                        error: Some(diag_from_error(&error, tool, &project, None)),
                    }
                }
            };
            items.push(item);
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

/// The window to write: the decided spec, raised to the binding floor when the floor is stricter.
fn effective_window(spec: &WindowSpec, floor: Option<SignedDuration>) -> WindowSpec {
    match (spec, floor) {
        (WindowSpec::MinAge(decided), Some(floor)) if floor > *decided => WindowSpec::MinAge(floor),
        _ => spec.clone(),
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
