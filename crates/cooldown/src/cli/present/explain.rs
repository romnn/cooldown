use crate::app;
use cooldown_render as render;

pub(in crate::cli) fn explain_meta(meta: &app::ExplainMeta) -> render::ExplainMeta {
    render::ExplainMeta {
        project: meta.project.clone(),
        registry: meta.registry.clone(),
        effective: render::EffectiveInfo {
            min_age_days: meta.effective.min_age_days,
            decided_by: meta.effective.decided_by.clone(),
        },
    }
}

pub(in crate::cli) fn explain_steps(steps: &[app::ExplainStep]) -> Vec<render::ExplainStep> {
    steps.iter().map(explain_step).collect()
}

fn explain_step(step: &app::ExplainStep) -> render::ExplainStep {
    render::ExplainStep {
        layer: step.layer.clone(),
        field: step.field.clone(),
        selector: step.selector.clone(),
        min_age_days: step.min_age_days,
        applied: step.applied,
        note: step.note.clone(),
    }
}
