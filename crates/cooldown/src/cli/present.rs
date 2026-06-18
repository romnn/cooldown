use crate::app;
use cooldown_core::{CoreError, Diagnostic, DiagnosticKind};
use cooldown_render as render;
use serde::Serialize;
use std::fmt::Write as _;

pub(super) fn no_tool_json(command: &'static str) -> Result<String, CoreError> {
    let error = Diagnostic::new(DiagnosticKind::NotFound, "no supported tool detected");
    let generated_at = super::generated_at(jiff::Timestamp::now());

    match command {
        "outdated" => no_tool_outdated(generated_at, error),
        "check" => no_tool_check(generated_at, error),
        "upgrade" => no_tool_upgrade(generated_at, error),
        "explain" => no_tool_explain(generated_at, error),
        "config" => no_tool_config(generated_at, error),
        "baseline" => no_tool_baseline(generated_at, error),
        _ => Err(CoreError::Config(format!(
            "command `{command}` does not produce a workspace JSON envelope"
        ))),
    }
}

fn no_tool_outdated(generated_at: String, error: Diagnostic) -> Result<String, CoreError> {
    serialize_no_tool(&with_error(
        render::Envelope::new(
            "outdated",
            false,
            generated_at,
            render::OutdatedMeta {},
            render::OutdatedSummary {
                total: 0,
                adoptable: 0,
                in_cooldown: 0,
                up_to_date: 0,
                exempt: 0,
                held: 0,
                unknown_age: 0,
                errors: 0,
            },
            Vec::<render::OutdatedItem>::new(),
        ),
        error,
    ))
}

fn no_tool_check(generated_at: String, error: Diagnostic) -> Result<String, CoreError> {
    serialize_no_tool(&with_error(
        render::Envelope::new(
            "check",
            false,
            generated_at,
            render::CheckMeta {
                scope: "lockfile-graph".into(),
                artifact_scope: "environment".into(),
            },
            render::CheckSummary {
                checked: 0,
                direct: 0,
                exempt: 0,
                acknowledged: 0,
                unknown_age: 0,
                errors: 0,
                violations: 0,
            },
            Vec::<render::CheckItem>::new(),
        ),
        error,
    ))
}

fn no_tool_upgrade(generated_at: String, error: Diagnostic) -> Result<String, CoreError> {
    serialize_no_tool(&with_error(
        render::Envelope::new(
            "upgrade",
            false,
            generated_at,
            render::UpgradeMeta {
                applied: false,
                lock_verified: None,
                build: render::BuildInfo {
                    requested: false,
                    ok: None,
                },
            },
            render::UpgradeSummary {
                applied: 0,
                skipped: 0,
                errors: 0,
            },
            Vec::<render::UpgradeItem>::new(),
        ),
        error,
    ))
}

fn no_tool_explain(generated_at: String, error: Diagnostic) -> Result<String, CoreError> {
    serialize_no_tool(&with_error(
        render::Envelope::new(
            "explain",
            false,
            generated_at,
            render::ExplainMeta {
                project: String::new(),
                registry: None,
                effective: render::EffectiveInfo {
                    min_age_days: 0.0,
                    decided_by: "default".into(),
                },
            },
            render::ExplainSummary {},
            Vec::<render::ExplainStep>::new(),
        ),
        error,
    ))
}

fn no_tool_config(generated_at: String, error: Diagnostic) -> Result<String, CoreError> {
    serialize_no_tool(&with_error(
        render::Envelope::new(
            "config",
            false,
            generated_at,
            render::ConfigMeta {},
            render::ConfigSummary { projects: 0 },
            Vec::<render::ConfigItem>::new(),
        ),
        error,
    ))
}

fn no_tool_baseline(generated_at: String, error: Diagnostic) -> Result<String, CoreError> {
    serialize_no_tool(&with_error(
        render::Envelope::new(
            "baseline",
            false,
            generated_at,
            render::BaselineMeta {
                path: String::new(),
            },
            render::BaselineSummary {
                acknowledged: 0,
                pruned: 0,
            },
            Vec::<render::BaselineItem>::new(),
        ),
        error,
    ))
}

fn serialize_no_tool<M, S, I>(env: &render::Envelope<M, S, I>) -> Result<String, CoreError>
where
    M: Serialize,
    S: Serialize,
    I: Serialize,
{
    render::to_json(env)
        .map_err(|error| CoreError::Serialization(format!("serialize JSON output: {error}")))
}

pub(super) fn render_config_text(items: &[app::ConfigItem]) -> String {
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

pub(super) fn outdated_summary(summary: &app::OutdatedSummary) -> render::OutdatedSummary {
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

pub(super) fn outdated_items(items: &[app::OutdatedItem]) -> Vec<render::OutdatedItem> {
    items.iter().map(outdated_item).collect()
}

pub(super) fn check_meta(meta: &app::CheckMeta) -> render::CheckMeta {
    render::CheckMeta {
        scope: meta.scope.clone(),
        artifact_scope: meta.artifact_scope.clone(),
    }
}

pub(super) fn check_summary(summary: &app::CheckSummary) -> render::CheckSummary {
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

pub(super) fn check_items(items: &[app::CheckItem]) -> Vec<render::CheckItem> {
    items.iter().map(check_item).collect()
}

pub(super) fn upgrade_meta(meta: &app::UpgradeMeta) -> render::UpgradeMeta {
    render::UpgradeMeta {
        applied: meta.applied,
        lock_verified: meta.lock_verified,
        build: render::BuildInfo {
            requested: meta.build.requested,
            ok: meta.build.ok,
        },
    }
}

pub(super) fn upgrade_summary(summary: &app::UpgradeSummary) -> render::UpgradeSummary {
    render::UpgradeSummary {
        applied: summary.applied,
        skipped: summary.skipped,
        errors: summary.errors,
    }
}

pub(super) fn upgrade_items(items: &[app::UpgradeItem]) -> Vec<render::UpgradeItem> {
    items.iter().map(upgrade_item).collect()
}

pub(super) fn explain_meta(meta: &app::ExplainMeta) -> render::ExplainMeta {
    render::ExplainMeta {
        project: meta.project.clone(),
        registry: meta.registry.clone(),
        effective: render::EffectiveInfo {
            min_age_days: meta.effective.min_age_days,
            decided_by: meta.effective.decided_by.clone(),
        },
    }
}

pub(super) fn explain_steps(steps: &[app::ExplainStep]) -> Vec<render::ExplainStep> {
    steps.iter().map(explain_step).collect()
}

pub(super) fn config_summary(summary: &app::ConfigSummary) -> render::ConfigSummary {
    render::ConfigSummary {
        projects: summary.projects,
    }
}

pub(super) fn config_items(items: &[app::ConfigItem]) -> Vec<render::ConfigItem> {
    items.iter().map(config_item).collect()
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

fn upgrade_item(item: &app::UpgradeItem) -> render::UpgradeItem {
    render::UpgradeItem {
        name: item.name.clone(),
        tool: item.tool.clone(),
        project: item.project.clone(),
        registry: item.registry.clone(),
        from: item.from.clone(),
        to: item.to.clone(),
        kind: item.kind,
        applied: item.applied,
        skipped: item.skipped.as_ref().map(skipped_info),
        error: item.error.clone(),
    }
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

fn latest_info(info: &app::LatestInfo) -> render::LatestInfo {
    render::LatestInfo {
        version: info.version.clone(),
        published_at: info.published_at.clone(),
        age_days: info.age_days,
    }
}

fn skipped_info(info: &app::SkippedInfo) -> render::SkippedInfo {
    render::SkippedInfo {
        reason: info.reason,
        message: info.message.clone(),
        offending: info.offending.clone(),
    }
}

fn window(window: &app::Window) -> render::Window {
    render::Window {
        min_age_days: window.min_age_days,
        source: window.source.clone(),
        clamped_by: window.clamped_by.clone(),
    }
}

fn outdated_status(status: app::OutdatedStatus) -> render::OutdatedStatus {
    match status {
        app::OutdatedStatus::UpToDate => render::OutdatedStatus::UpToDate,
        app::OutdatedStatus::Adoptable => render::OutdatedStatus::Adoptable,
        app::OutdatedStatus::InCooldown => render::OutdatedStatus::InCooldown,
        app::OutdatedStatus::Exempt => render::OutdatedStatus::Exempt,
        app::OutdatedStatus::Held => render::OutdatedStatus::Held,
        app::OutdatedStatus::CurrentInCooldown => render::OutdatedStatus::CurrentInCooldown,
        app::OutdatedStatus::UnknownAge => render::OutdatedStatus::UnknownAge,
        app::OutdatedStatus::Error => render::OutdatedStatus::Error,
    }
}

fn check_status(status: app::CheckStatus) -> render::CheckStatus {
    match status {
        app::CheckStatus::Violation => render::CheckStatus::Violation,
        app::CheckStatus::Acknowledged => render::CheckStatus::Acknowledged,
        app::CheckStatus::UnknownAge => render::CheckStatus::UnknownAge,
        app::CheckStatus::Error => render::CheckStatus::Error,
    }
}

fn with_error<M, S, I>(
    mut env: render::Envelope<M, S, I>,
    error: Diagnostic,
) -> render::Envelope<M, S, I>
where
    M: Serialize,
    S: Serialize,
    I: Serialize,
{
    env.errors.push(error);
    env
}
