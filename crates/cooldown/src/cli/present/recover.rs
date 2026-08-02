use crate::app::{RecoveryItem, RecoverySummary};
use serde::Serialize;
use std::fmt::Write as _;

/// The `recover` JSON metadata object.
#[derive(Serialize, Clone)]
pub(in crate::cli) struct RecoveryMeta {}

/// Per-status recovery counts in JSON output.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(in crate::cli) struct RecoverySummaryJson {
    recovered: usize,
    unchanged: usize,
    errors: usize,
}

/// One project recovery result in JSON output.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(in crate::cli) struct RecoveryItemJson {
    tool: String,
    project: String,
    status: String,
}

pub(in crate::cli) fn recovery_summary(summary: &RecoverySummary) -> RecoverySummaryJson {
    RecoverySummaryJson {
        recovered: summary.recovered,
        unchanged: summary.unchanged,
        errors: summary.errors,
    }
}

pub(in crate::cli) fn recovery_items(items: &[RecoveryItem]) -> Vec<RecoveryItemJson> {
    items
        .iter()
        .map(|item| RecoveryItemJson {
            tool: item.tool.clone(),
            project: item.project.clone(),
            status: item.status.token().to_string(),
        })
        .collect()
}

/// Renders the recovery-only report without exposing or continuing into another mutation.
pub(in crate::cli) fn render_recovery_text(
    summary: &RecoverySummary,
    items: &[RecoveryItem],
) -> String {
    let mut out = String::new();
    for item in items {
        let _ = writeln!(
            out,
            "  {} ({}): {}",
            item.project,
            item.tool,
            item.status.token()
        );
    }
    if !items.is_empty() {
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "{} recovered · {} unchanged · {} errors",
        summary.recovered, summary.unchanged, summary.errors
    );
    out
}
