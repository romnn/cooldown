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
