use crate::app::{RecoveryItem, RecoverySummary};
use cooldown_render::{
    RecoveryItem as RecoveryItemJson, RecoveryStatus as RecoveryStatusJson,
    RecoverySummary as RecoverySummaryJson,
};
use std::fmt::Write as _;

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
            status: match item.status {
                crate::app::RecoveryStatus::Recovered => RecoveryStatusJson::Recovered,
                crate::app::RecoveryStatus::Unchanged => RecoveryStatusJson::Unchanged,
                crate::app::RecoveryStatus::Error => RecoveryStatusJson::Error,
            },
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
        if let Some(error) = &item.error {
            let _ = writeln!(out, "    error [{}]: {}", error.kind, error.message);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::RecoveryStatus;
    use cooldown_core::{Diagnostic, DiagnosticKind};

    #[test]
    fn recovery_error_text_includes_the_actionable_diagnostic() {
        let item = RecoveryItem {
            tool: "cargo".to_string(),
            project: ".".to_string(),
            status: RecoveryStatus::Error,
            error: Some(Diagnostic::new(
                DiagnosticKind::LockConflict,
                "Cargo.lock changed independently; left recovery state untouched",
            )),
        };

        let rendered = render_recovery_text(
            &RecoverySummary {
                recovered: 0,
                unchanged: 0,
                errors: 1,
            },
            &[item],
        );

        assert!(rendered.contains(". (cargo): error"));
        assert!(rendered.contains(
            "error [lock_conflict]: Cargo.lock changed independently; left recovery state untouched"
        ));
    }
}
