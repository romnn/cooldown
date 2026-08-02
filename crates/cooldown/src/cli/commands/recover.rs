use super::common::{emit_envelope, with_diags};
use crate::app::{Exit, recover_targets};
use crate::cli::{present, runtime, setup};
use cooldown_core::CoreError;
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
    let envelope = with_diags(
        render::Envelope::new(
            "recover",
            outcome.exit.is_ok(),
            runtime::generated_at(jiff::Timestamp::now()),
            render::RecoveryMeta {},
            summary,
            items,
        ),
        Vec::new(),
        errors,
    );
    emit_envelope(prepared.json, &envelope, || {
        present::render_recovery_text(&outcome.summary, &outcome.items)
    })?;
    Ok(outcome.exit)
}
