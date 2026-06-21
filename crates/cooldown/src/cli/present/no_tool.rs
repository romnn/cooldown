use super::common::{serialize_no_tool, with_error};
use cooldown_core::{CoreError, Diagnostic, DiagnosticKind};
use cooldown_render as render;

pub(in crate::cli) fn no_tool_json(command: &'static str) -> Result<String, CoreError> {
    let error = Diagnostic::new(DiagnosticKind::NotFound, "no supported tool detected");
    let generated_at = super::super::generated_at(jiff::Timestamp::now());

    match command {
        "outdated" => no_tool_outdated(generated_at, error),
        "check" => no_tool_check(generated_at, error),
        "upgrade" | "fix" => no_tool_mutation(command, generated_at, error),
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
                allowed: 0,
                unknown_age: 0,
                errors: 0,
                violations: 0,
            },
            Vec::<render::CheckItem>::new(),
        ),
        error,
    ))
}

fn no_tool_mutation(
    command: &'static str,
    generated_at: String,
    error: Diagnostic,
) -> Result<String, CoreError> {
    serialize_no_tool(&with_error(
        render::Envelope::new(
            command,
            false,
            generated_at,
            render::UpgradeMeta {
                applied: false,
                lock_verified: None,
                build: render::BuildInfo {
                    requested: false,
                    ok: None,
                },
                major_available: Vec::new(),
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
                dry_run: false,
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
