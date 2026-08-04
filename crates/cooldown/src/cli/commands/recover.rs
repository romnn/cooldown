use super::common::{emit_envelope, with_diags};
use crate::app::{Exit, recover_targets};
use crate::cli::{present, runtime, setup};
use cooldown_core::{CoreError, Diagnostic};
use cooldown_render as render;

pub(super) fn run(prepared: setup::PreparedRecovery) -> Result<Exit, CoreError> {
    let tools: Vec<_> = prepared.targets.iter().map(|target| target.tool).collect();
    prepared.progress.start_run(&tools);
    let outcome = recover_targets(prepared.targets, &prepared.progress);
    prepared.progress.finish_run();

    let summary = present::recovery_summary(&outcome.summary);
    let items = present::recovery_items(&outcome.items);
    let errors = outcome
        .items
        .iter()
        .filter_map(|item| item.error.clone())
        .collect();
    let warnings = outcome
        .items
        .iter()
        .flat_map(|item| item.warnings.clone())
        .collect();
    let envelope = with_diags(
        render::Envelope::new(
            "recover",
            outcome.exit.is_ok(),
            runtime::generated_at(jiff::Timestamp::now()),
            render::RecoveryMeta {},
            summary,
            items,
        ),
        warnings,
        errors,
    );
    emit_envelope(prepared.json, &envelope, || {
        present::render_recovery_text(&outcome.summary, &outcome.items)
    })?;
    Ok(outcome.exit)
}

pub(super) fn run_preparation_error(error: &CoreError, exit: Exit) -> Result<Exit, CoreError> {
    let envelope = with_diags(
        render::Envelope::new(
            "recover",
            false,
            runtime::generated_at(jiff::Timestamp::now()),
            render::RecoveryMeta {},
            render::RecoverySummary {
                recovered: 0,
                unchanged: 0,
                errors: 1,
            },
            Vec::<render::RecoveryItem>::new(),
        ),
        Vec::new(),
        vec![Diagnostic::new(error.diagnostic_kind(), error.to_string())],
    );
    emit_envelope(true, &envelope, String::new)?;
    Ok(exit)
}
