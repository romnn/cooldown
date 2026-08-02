//! `recover` — restore adapter-owned project state left by an interrupted mutation, then stop.

use super::lock::ProjectWriteGuard;
use super::{Exit, RunOpts, Workspace, diag_from_error};
use cooldown_core::Diagnostic;

/// What happened while recovering one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    /// Interrupted state was validated and restored.
    Recovered,
    /// No interrupted state was present.
    Unchanged,
    /// Recovery could not safely complete.
    Error,
}

impl RecoveryStatus {
    /// The lowercase token used in text and JSON output.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            RecoveryStatus::Recovered => "recovered",
            RecoveryStatus::Unchanged => "unchanged",
            RecoveryStatus::Error => "error",
        }
    }
}

/// One project's recovery result.
#[derive(Debug, Clone)]
pub struct RecoveryItem {
    /// The project tool.
    pub tool: String,
    /// The project, relative to the repository root.
    pub project: String,
    /// The recovery outcome.
    pub status: RecoveryStatus,
    /// The diagnostic when recovery failed.
    pub error: Option<Diagnostic>,
}

/// Per-status recovery counts.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoverySummary {
    /// Projects whose interrupted state was restored.
    pub recovered: usize,
    /// Projects without interrupted state.
    pub unchanged: usize,
    /// Projects that could not be safely recovered.
    pub errors: usize,
}

/// The result of a recovery-only run.
pub struct RecoveryOutcome {
    /// Per-status counts.
    pub summary: RecoverySummary,
    /// Per-project recovery results.
    pub items: Vec<RecoveryItem>,
    /// The process exit: non-zero if any project failed recovery.
    pub exit: Exit,
}

impl Workspace {
    /// Recovers interrupted adapter mutations under exclusive project access without continuing
    /// into dependency resolution or another mutation.
    pub async fn recover(&self, opts: &RunOpts) -> RecoveryOutcome {
        let mut summary = RecoverySummary::default();
        let mut items = Vec::new();
        for pctx in self.scoped_projects(opts) {
            let _progress = opts.progress.project(pctx.tool, pctx.rel_path.as_str());
            opts.progress.phase("checking interrupted project state");
            let project = pctx.rel_path.to_string();
            let Some(writer) = self.mutator(pctx.tool) else {
                summary.unchanged += 1;
                items.push(RecoveryItem {
                    tool: pctx.tool.as_str().to_string(),
                    project,
                    status: RecoveryStatus::Unchanged,
                    error: None,
                });
                continue;
            };
            let result = match ProjectWriteGuard::acquire(&pctx.project.root) {
                Ok(_guard) => writer.recover_pending_mutation(&pctx.project).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(true) => {
                    summary.recovered += 1;
                    items.push(RecoveryItem {
                        tool: pctx.tool.as_str().to_string(),
                        project,
                        status: RecoveryStatus::Recovered,
                        error: None,
                    });
                }
                Ok(false) => {
                    summary.unchanged += 1;
                    items.push(RecoveryItem {
                        tool: pctx.tool.as_str().to_string(),
                        project,
                        status: RecoveryStatus::Unchanged,
                        error: None,
                    });
                }
                Err(error) => {
                    summary.errors += 1;
                    let diagnostic = diag_from_error(&error, pctx.tool, &project, None);
                    items.push(RecoveryItem {
                        tool: pctx.tool.as_str().to_string(),
                        project,
                        status: RecoveryStatus::Error,
                        error: Some(diagnostic),
                    });
                }
            }
        }
        items.sort_by(|a, b| a.project.cmp(&b.project).then_with(|| a.tool.cmp(&b.tool)));
        RecoveryOutcome {
            summary,
            items,
            exit: if summary.errors == 0 {
                Exit::Ok
            } else {
                Exit::Environment
            },
        }
    }
}
