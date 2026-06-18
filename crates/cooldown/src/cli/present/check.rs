use super::common::{check_status, window};
use crate::app;
use cooldown_render as render;

pub(in crate::cli) fn check_meta(meta: &app::CheckMeta) -> render::CheckMeta {
    render::CheckMeta {
        scope: meta.scope.clone(),
        artifact_scope: meta.artifact_scope.clone(),
    }
}

pub(in crate::cli) fn check_summary(summary: &app::CheckSummary) -> render::CheckSummary {
    render::CheckSummary {
        checked: summary.checked,
        direct: summary.direct,
        exempt: summary.exempt,
        acknowledged: summary.acknowledged,
        unknown_age: summary.unknown_age,
        errors: summary.errors,
        violations: summary.violations,
    }
}

pub(in crate::cli) fn check_items(items: &[app::CheckItem]) -> Vec<render::CheckItem> {
    items.iter().map(check_item).collect()
}

fn check_item(item: &app::CheckItem) -> render::CheckItem {
    render::CheckItem {
        name: item.name.clone(),
        tool: item.tool.clone(),
        project: item.project.clone(),
        registry: item.registry.clone(),
        direct: item.direct,
        current: item.current.clone(),
        published_at: item.published_at.clone(),
        age_days: item.age_days,
        window: window(&item.window),
        status: check_status(item.status),
        graph_held: item.graph_held,
        graph_floor: item.graph_floor.clone(),
        error: item.error.clone(),
    }
}
