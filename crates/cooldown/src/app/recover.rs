//! `recover` — settle adapter-owned project state left by an interrupted mutation, then stop.

use super::progress::Progress;
use super::{Exit, diag_from_error};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{Diagnostic, MutationRecovery, RecoveryDisposition, ToolId};

/// What happened while recovering one project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    /// A completely published accepted candidate was retained.
    Accepted,
    /// A partial or speculative mutation was restored to its preimage.
    Restored,
    /// Only recovery artifacts remained and were consumed.
    CleanupOnly,
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
            RecoveryStatus::Accepted => "accepted",
            RecoveryStatus::Restored => "restored",
            RecoveryStatus::CleanupOnly => "cleanup-only",
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
    /// Non-fatal durability or cleanup diagnostics.
    pub warnings: Vec<Diagnostic>,
}

/// Per-status recovery counts.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecoverySummary {
    /// Projects whose interrupted state was settled as accepted, restored, or cleanup-only.
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

type RecoverProject = fn(&Utf8Path) -> cooldown_core::Result<MutationRecovery>;

/// A project found from adapter-owned recovery artifacts without normal policy bootstrap.
pub(crate) struct RecoveryTarget {
    pub(crate) tool: ToolId,
    pub(crate) root: Utf8PathBuf,
    pub(crate) project: String,
    recover: RecoverProject,
}

impl RecoveryTarget {
    pub(crate) fn new(
        tool: ToolId,
        root: Utf8PathBuf,
        project: String,
        recover: RecoverProject,
    ) -> Self {
        RecoveryTarget {
            tool,
            root,
            project,
            recover,
        }
    }
}

/// Recovers pre-discovered targets without loading policy, manifests, baselines, or registries.
pub(crate) fn recover_targets(
    targets: Vec<RecoveryTarget>,
    progress: &Progress,
) -> RecoveryOutcome {
    let mut summary = RecoverySummary::default();
    let mut items = Vec::new();
    for target in targets {
        let _progress = progress.project(target.tool, &target.project);
        progress.phase("checking interrupted project state");
        let result = (target.recover)(&target.root);
        let (status, error, warnings) = match result {
            Ok(recovery) => {
                let status = match recovery.disposition {
                    RecoveryDisposition::Unchanged => {
                        summary.unchanged += 1;
                        RecoveryStatus::Unchanged
                    }
                    RecoveryDisposition::Accepted => {
                        summary.recovered += 1;
                        RecoveryStatus::Accepted
                    }
                    RecoveryDisposition::Restored => {
                        summary.recovered += 1;
                        RecoveryStatus::Restored
                    }
                    RecoveryDisposition::CleanupOnly => {
                        summary.recovered += 1;
                        RecoveryStatus::CleanupOnly
                    }
                };
                let warnings = recovery
                    .warnings
                    .into_iter()
                    .map(|warning| {
                        warning
                            .with_tool(target.tool.as_str())
                            .with_project(&target.project)
                    })
                    .collect();
                (status, None, warnings)
            }
            Err(error) => {
                summary.errors += 1;
                (
                    RecoveryStatus::Error,
                    Some(diag_from_error(&error, target.tool, &target.project, None)),
                    Vec::new(),
                )
            }
        };
        items.push(RecoveryItem {
            tool: target.tool.as_str().to_string(),
            project: target.project,
            status,
            error,
            warnings,
        });
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
