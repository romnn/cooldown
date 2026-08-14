use crate::app;
use std::fmt::Write as _;

pub(in crate::cli) fn render_config_text(items: &[app::ConfigItem]) -> String {
    let mut text = String::new();
    for item in items {
        let _ = writeln!(
            text,
            "{} [{}]\n  effective default window: {}d (decided by {})\n  strict-native: {}\n  layers: {}\n  advisories: {}",
            item.project,
            item.tool,
            item.effective_default_min_age_days,
            item.source,
            item.strict_native,
            item.layers.join(" < "),
            advisories_line(&item.advisories),
        );
    }
    text
}

/// The `[advisories]` one-liner: the resolved policy plus this tool's feed coverage, so a run
/// that never annotates anything says why (disabled, or no ecosystem covers the tool).
fn advisories_line(advisories: &app::AdvisoryConfigInfo) -> String {
    if !advisories.enabled {
        return "disabled".to_string();
    }
    let coverage = match &advisories.ecosystem {
        Some(ecosystem) => format!("ecosystem {ecosystem}"),
        None => "no ecosystem covers this tool".to_string(),
    };
    format!(
        "{} via {} · mode {} · security window {}d · severity ≥ {} · {coverage}",
        "enabled", advisories.source, advisories.mode, advisories.min_age_days, advisories.severity,
    )
}
