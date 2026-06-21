use super::common::skipped_info;
use crate::app;
use cooldown_render as render;

pub(in crate::cli) fn upgrade_meta(meta: &app::UpgradeMeta) -> render::UpgradeMeta {
    render::UpgradeMeta {
        applied: meta.applied,
        lock_verified: meta.lock_verified,
        build: render::BuildInfo {
            requested: meta.build.requested,
            ok: meta.build.ok,
        },
    }
}

pub(in crate::cli) fn upgrade_summary(summary: &app::UpgradeSummary) -> render::UpgradeSummary {
    render::UpgradeSummary {
        applied: summary.applied,
        skipped: summary.skipped,
        errors: summary.errors,
    }
}

pub(in crate::cli) fn upgrade_items(items: &[app::UpgradeItem]) -> Vec<render::UpgradeItem> {
    items.iter().map(upgrade_item).collect()
}

fn upgrade_item(item: &app::UpgradeItem) -> render::UpgradeItem {
    render::UpgradeItem {
        name: item.name.clone(),
        tool: item.tool.clone(),
        project: item.project.clone(),
        direct: item.direct,
        downgrade: item.downgrade,
        members: item.members.clone(),
        registry: item.registry.clone(),
        from: item.from.clone(),
        to: item.to.clone(),
        kind: item.kind,
        applied: item.applied,
        skipped: item.skipped.as_ref().map(skipped_info),
        error: item.error.clone(),
    }
}
