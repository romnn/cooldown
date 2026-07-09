use crate::app;
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
