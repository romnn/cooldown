use crate::app::{SyncItem, SyncSummary};
use serde::Serialize;
use std::fmt::Write as _;

/// The `sync` JSON `meta` (no fields today; present for envelope shape consistency).
#[derive(Serialize, Clone)]
pub(in crate::cli) struct SyncMeta {}

/// The `sync` JSON summary: per-status project counts.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(in crate::cli) struct SyncSummaryJson {
    written: usize,
    unchanged: usize,
    unsupported: usize,
    errors: usize,
}

/// One `sync` JSON item.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(in crate::cli) struct SyncItemJson {
    tool: String,
    project: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    window: Option<String>,
}

pub(in crate::cli) fn sync_summary(summary: &SyncSummary) -> SyncSummaryJson {
    SyncSummaryJson {
        written: summary.written,
        unchanged: summary.unchanged,
        unsupported: summary.unsupported,
        errors: summary.errors,
    }
}

pub(in crate::cli) fn sync_items(items: &[SyncItem]) -> Vec<SyncItemJson> {
    items
        .iter()
        .map(|item| SyncItemJson {
            tool: item.tool.clone(),
            project: item.project.clone(),
            status: item.status.token().to_string(),
            path: item.path.clone(),
            window: item.window.clone(),
        })
        .collect()
}

/// Render the human-readable `sync` report: a line per project plus the summary tally.
pub(in crate::cli) fn render_sync_text(summary: &SyncSummary, items: &[SyncItem]) -> String {
    let mut out = String::new();
    for item in items {
        let window = match &item.window {
            Some(window) => format!(" [{window}]"),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "  {} ({}): {}{}",
            item.project,
            item.tool,
            item.status.token(),
            window
        );
    }
    if !items.is_empty() {
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "{} written · {} unchanged · {} unsupported · {} errors",
        summary.written, summary.unchanged, summary.unsupported, summary.errors
    );
    out
}
