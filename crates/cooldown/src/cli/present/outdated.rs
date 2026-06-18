use super::common::{latest_info, outdated_status, window};
use crate::app;
use cooldown_render as render;

pub(in crate::cli) fn outdated_summary(summary: &app::OutdatedSummary) -> render::OutdatedSummary {
    render::OutdatedSummary {
        total: summary.total,
        adoptable: summary.adoptable,
        in_cooldown: summary.in_cooldown,
        up_to_date: summary.up_to_date,
        exempt: summary.exempt,
        held: summary.held,
        unknown_age: summary.unknown_age,
        errors: summary.errors,
    }
}

pub(in crate::cli) fn outdated_items(items: &[app::OutdatedItem]) -> Vec<render::OutdatedItem> {
    items.iter().map(outdated_item).collect()
}

fn outdated_item(item: &app::OutdatedItem) -> render::OutdatedItem {
    render::OutdatedItem {
        name: item.name.clone(),
        tool: item.tool.clone(),
        project: item.project.clone(),
        registry: item.registry.clone(),
        direct: item.direct,
        current: item.current.clone(),
        window: window(&item.window),
        status: outdated_status(item.status),
        adoptable_target: item.adoptable_target.clone(),
        latest: item.latest.as_ref().map(latest_info),
        error: item.error.clone(),
    }
}
