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
        if let Some(generated) = &item.generated_members {
            let _ = writeln!(
                text,
                "  generated members: {}",
                generated_members_line(generated)
            );
        }
    }
    text
}

/// The `generated-members` one-liner: the declared members and the file that declared them, or
/// the fact that nothing declares the key — each a different thing for an audit to know.
fn generated_members_line(generated: &app::GeneratedMembersInfo) -> String {
    match (&generated.origin, generated.names.as_slice()) {
        (None, _) => "none declared (every member is authored)".to_string(),
        (Some(origin), []) => format!("none (declared by {origin})"),
        (Some(origin), names) => format!("{} (declared by {origin})", names.join(", ")),
    }
}

/// The `[advisories]` one-liner: the resolved policy plus this tool's feed coverage, so a run
/// that never annotates anything says why (disabled, or no safe project-wide ecosystem mapping).
fn advisories_line(advisories: &app::AdvisoryConfigInfo) -> String {
    if !advisories.enabled {
        return "disabled".to_string();
    }
    let coverage = match &advisories.ecosystem {
        Some(ecosystem) => format!("ecosystem {ecosystem}"),
        None => "no single ecosystem maps this tool's package graph".to_string(),
    };
    format!(
        "enabled via {} · mode {} · security window {}d · severity ≥ {} · {coverage}",
        advisories.source, advisories.mode, advisories.min_age_days, advisories.severity,
    )
}
