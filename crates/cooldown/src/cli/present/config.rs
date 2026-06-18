use crate::app;
use cooldown_render as render;
use std::fmt::Write as _;

pub(in crate::cli) fn render_config_text(items: &[app::ConfigItem]) -> String {
    let mut text = String::new();
    for item in items {
        let _ = writeln!(
            text,
            "{} [{}]\n  effective default window: {}d (decided by {})\n  strict-native: {}\n  layers: {}",
            item.project,
            item.tool,
            item.effective_default_min_age_days,
            item.source,
            item.strict_native,
            item.layers.join(" < "),
        );
    }
    text
}

pub(in crate::cli) fn config_summary(summary: &app::ConfigSummary) -> render::ConfigSummary {
    render::ConfigSummary {
        projects: summary.projects,
    }
}

pub(in crate::cli) fn config_items(items: &[app::ConfigItem]) -> Vec<render::ConfigItem> {
    items.iter().map(config_item).collect()
}

fn config_item(item: &app::ConfigItem) -> render::ConfigItem {
    render::ConfigItem {
        project: item.project.clone(),
        tool: item.tool.clone(),
        effective_default_min_age_days: item.effective_default_min_age_days,
        source: item.source.clone(),
        strict_native: item.strict_native,
        layers: item.layers.clone(),
    }
}
