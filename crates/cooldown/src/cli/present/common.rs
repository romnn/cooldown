use crate::app;
use cooldown_core::{CoreError, Diagnostic};
use cooldown_render as render;
use serde::Serialize;

pub(super) fn serialize_no_tool<M, S, I>(
    env: &render::Envelope<M, S, I>,
) -> Result<String, CoreError>
where
    M: Serialize,
    S: Serialize,
    I: Serialize,
{
    render::to_json(env)
        .map_err(|error| CoreError::Serialization(format!("serialize JSON output: {error}")))
}

pub(super) fn latest_info(info: &app::LatestInfo) -> render::LatestInfo {
    render::LatestInfo {
        version: info.version.clone(),
        published_at: info.published_at.clone(),
        age_days: info.age_days,
    }
}

pub(super) fn skipped_info(info: &app::SkippedInfo) -> render::SkippedInfo {
    render::SkippedInfo {
        reason: info.reason,
        message: info.message.clone(),
        offending: info.offending.clone(),
    }
}

pub(super) fn window(window: &app::Window) -> render::Window {
    render::Window {
        min_age_days: window.min_age_days,
        source: window.source.clone(),
        clamped_by: window.clamped_by.clone(),
    }
}

pub(super) fn outdated_status(status: app::OutdatedStatus) -> render::OutdatedStatus {
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

pub(super) fn check_status(status: app::CheckStatus) -> render::CheckStatus {
    match status {
        app::CheckStatus::Violation => render::CheckStatus::Violation,
        app::CheckStatus::Acknowledged => render::CheckStatus::Acknowledged,
        app::CheckStatus::Allowed => render::CheckStatus::Allowed,
        app::CheckStatus::UnknownAge => render::CheckStatus::UnknownAge,
        app::CheckStatus::Error => render::CheckStatus::Error,
    }
}

pub(super) fn with_error<M, S, I>(
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
