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
        major_available: meta.major_available.iter().map(major_update).collect(),
    }
}

fn major_update(update: &app::MajorUpdate) -> render::MajorUpdate {
    render::MajorUpdate {
        name: update.name.clone(),
        project: update.project.clone(),
        from: update.from.clone(),
        to: update.to.clone(),
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
