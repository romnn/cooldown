use cooldown_core::CoreError;
use cooldown_render as render;
use serde::Serialize;

pub(super) fn emit_envelope<M, S, I, F>(
    json: bool,
    env: &render::Envelope<M, S, I>,
    render_tty: F,
) -> Result<(), CoreError>
where
    M: Serialize,
    S: Serialize,
    I: Serialize,
    F: FnOnce() -> String,
{
    if json {
        let json = render::to_json(env)
            .map_err(|error| CoreError::Serialization(format!("serialize JSON output: {error}")))?;
        crate::cli::output::stdout_line(&json)?;
    } else {
        crate::cli::output::stdout(&render_tty())?;
    }
    Ok(())
}

pub(super) fn with_diags<M, S, I>(
    mut env: render::Envelope<M, S, I>,
    warnings: Vec<cooldown_core::Diagnostic>,
    errors: Vec<cooldown_core::Diagnostic>,
) -> render::Envelope<M, S, I>
where
    M: Serialize,
    S: Serialize,
    I: Serialize,
{
    env.warnings = warnings;
    env.errors = errors;
    env
}
