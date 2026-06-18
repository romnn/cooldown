use super::CommandContext;
use super::common::{emit_envelope, with_diags};
use crate::app::Exit;
use crate::cli::present;
use cooldown_core::CoreError;
use cooldown_render as render;

pub(super) async fn run_outdated(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.outdated(ctx.opts).await;
    // `--exit-code N` turns the informational report into a CI gate: a non-zero exit when there is
    // at least one adoptable update (a newer version that has already cleared its cooldown window).
    // Items still in cooldown never trip it — they aren't adoptable yet, by design.
    let exit = match ctx.opts.outdated_exit_code {
        Some(code) if code != 0 && out.summary.adoptable > 0 => Exit::Gated(code),
        _ => out.exit,
    };
    let summary = present::outdated_summary(&out.summary);
    let items = present::outdated_items(&out.items);
    let env = with_diags(
        render::Envelope::new(
            "outdated",
            exit.is_ok(),
            ctx.generated_at.to_owned(),
            render::OutdatedMeta {},
            summary.clone(),
            items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    // The JSON envelope keeps every item (machine consumers filter themselves); the human table
    // hides up-to-date rows unless `--all`, so the common case is a short, actionable report. The
    // summary line below the table still reflects all dependencies.
    let table_items: Vec<render::OutdatedItem> = if ctx.global.all {
        items.clone()
    } else {
        items
            .iter()
            .filter(|i| i.status != render::OutdatedStatus::UpToDate)
            .cloned()
            .collect()
    };
    emit_envelope(ctx.global.json, &env, || {
        render::tty::render_outdated(
            &summary,
            &table_items,
            &out.warnings,
            &out.errors,
            ctx.color,
        )
    })?;
    Ok(exit)
}

pub(super) async fn run_check(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.check(ctx.opts).await;
    let meta = present::check_meta(&out.meta);
    let summary = present::check_summary(&out.summary);
    let items = present::check_items(&out.items);
    let env = with_diags(
        render::Envelope::new(
            "check",
            out.exit.is_ok(),
            ctx.generated_at.to_owned(),
            meta.clone(),
            summary.clone(),
            items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(ctx.global.json, &env, || {
        render::tty::render_check(
            &meta,
            &summary,
            &items,
            &out.warnings,
            &out.errors,
            ctx.color,
        )
    })?;
    Ok(out.exit)
}

pub(super) async fn run_upgrade(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    if ctx.global.include_indirect {
        return Err(CoreError::Config(
            "`upgrade --include-indirect` is not allowed: acting on transitive deps is a non-goal"
                .into(),
        ));
    }
    if ctx.global.major && ctx.global.package.is_empty() && !ctx.global.major_all {
        return Err(CoreError::Config(
            "`upgrade --major` rewrites import paths repo-wide; pass --package or --major-all"
                .into(),
        ));
    }
    let out = ctx.ws.upgrade(ctx.opts).await;
    let meta = present::upgrade_meta(&out.meta);
    let summary = present::upgrade_summary(&out.summary);
    let items = present::upgrade_items(&out.items);
    let env = with_diags(
        render::Envelope::new(
            "upgrade",
            out.exit.is_ok(),
            ctx.generated_at.to_owned(),
            meta.clone(),
            summary.clone(),
            items.clone(),
        ),
        out.warnings.clone(),
        out.errors.clone(),
    );
    emit_envelope(ctx.global.json, &env, || {
        render::tty::render_upgrade(
            &meta,
            &summary,
            &items,
            &out.warnings,
            &out.errors,
            ctx.color,
        )
    })?;
    Ok(out.exit)
}

pub(super) async fn run_explain(
    ctx: &CommandContext<'_>,
    package: &str,
) -> Result<Exit, CoreError> {
    let out = ctx.ws.explain(package, ctx.opts).await;
    let meta = present::explain_meta(&out.meta);
    let steps = present::explain_steps(&out.steps);
    let env = render::Envelope::new(
        "explain",
        out.exit.is_ok(),
        ctx.generated_at.to_owned(),
        meta.clone(),
        render::ExplainSummary {},
        steps.clone(),
    );
    emit_envelope(ctx.global.json, &env, || {
        render::tty::render_explain(&meta, &steps, ctx.color)
    })?;
    Ok(out.exit)
}

pub(super) fn run_config(ctx: &CommandContext<'_>) -> Result<Exit, CoreError> {
    let out = ctx.ws.config(ctx.opts);
    let summary = present::config_summary(&out.summary);
    let items = present::config_items(&out.items);
    let env = render::Envelope::new(
        "config",
        out.exit.is_ok(),
        ctx.generated_at.to_owned(),
        render::ConfigMeta {},
        summary.clone(),
        items.clone(),
    );
    emit_envelope(ctx.global.json, &env, || {
        present::render_config_text(&out.items)
    })?;
    Ok(out.exit)
}
